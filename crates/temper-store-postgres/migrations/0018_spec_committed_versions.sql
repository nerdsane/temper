-- The last committed version of a spec whose row now holds an uncommitted
-- one. Startup recovery puts it back, so a failed or interrupted reinstall
-- never costs a type its working version. Cleared when the row commits.
CREATE TABLE IF NOT EXISTS spec_committed_versions (
    tenant              TEXT         NOT NULL,
    entity_type         TEXT         NOT NULL,
    ioa_source          TEXT         NOT NULL,
    csdl_xml            TEXT,
    content_hash        TEXT         NOT NULL,
    version             INT          NOT NULL,
    verified            BOOLEAN      NOT NULL,
    verification_status TEXT         NOT NULL,
    levels_passed       INT,
    levels_total        INT,
    verification_result JSONB,
    updated_at          TIMESTAMPTZ  NOT NULL,
    PRIMARY KEY (tenant, entity_type)
);

ALTER TABLE spec_committed_versions ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON spec_committed_versions;
CREATE POLICY tenant_isolation ON spec_committed_versions USING (tenant = current_setting('app.current_tenant', true));
