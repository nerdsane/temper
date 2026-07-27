-- ARN-190: non-reusable spec generations and last-known-good staging.

ALTER TABLE specs ALTER COLUMN version TYPE BIGINT;
ALTER TABLE tenant_constraints ALTER COLUMN version TYPE BIGINT;

CREATE TABLE IF NOT EXISTS spec_source_generations (
    tenant       TEXT   NOT NULL,
    entity_type  TEXT   NOT NULL,
    generation   BIGINT NOT NULL CHECK (generation > 0),
    PRIMARY KEY (tenant, entity_type)
);

INSERT INTO spec_source_generations (tenant, entity_type, generation)
SELECT tenant, entity_type, version FROM specs
ON CONFLICT (tenant, entity_type) DO UPDATE
SET generation = GREATEST(spec_source_generations.generation, EXCLUDED.generation);

CREATE TABLE IF NOT EXISTS spec_staging (
    tenant              TEXT        NOT NULL,
    entity_type         TEXT        NOT NULL,
    ioa_source          TEXT        NOT NULL,
    csdl_xml            TEXT,
    content_hash        TEXT        NOT NULL DEFAULT '',
    version             BIGINT      NOT NULL CHECK (version > 0),
    verified            BOOLEAN     NOT NULL DEFAULT false,
    verification_status TEXT        NOT NULL DEFAULT 'pending',
    levels_passed       INT,
    levels_total        INT,
    verification_result JSONB,
    staged_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type)
);

-- Preserve legacy uncommitted candidates as staging during upgrade, then
-- restore `specs` to the invariant that it contains committed source only.
INSERT INTO spec_staging (
    tenant, entity_type, ioa_source, csdl_xml, content_hash, version,
    verified, verification_status, levels_passed, levels_total,
    verification_result, staged_at
)
SELECT tenant, entity_type, ioa_source, csdl_xml, content_hash, version,
       verified, verification_status, levels_passed, levels_total,
       verification_result, updated_at
FROM specs WHERE committed = false
ON CONFLICT (tenant, entity_type) DO UPDATE SET
    ioa_source = EXCLUDED.ioa_source,
    csdl_xml = EXCLUDED.csdl_xml,
    content_hash = EXCLUDED.content_hash,
    version = EXCLUDED.version,
    verified = EXCLUDED.verified,
    verification_status = EXCLUDED.verification_status,
    levels_passed = EXCLUDED.levels_passed,
    levels_total = EXCLUDED.levels_total,
    verification_result = EXCLUDED.verification_result,
    staged_at = EXCLUDED.staged_at;
DELETE FROM specs WHERE committed = false;

CREATE TABLE IF NOT EXISTS tenant_constraint_generations (
    tenant      TEXT   NOT NULL PRIMARY KEY,
    generation  BIGINT NOT NULL CHECK (generation > 0)
);

INSERT INTO tenant_constraint_generations (tenant, generation)
SELECT tenant, version FROM tenant_constraints
ON CONFLICT (tenant) DO UPDATE
SET generation = GREATEST(tenant_constraint_generations.generation, EXCLUDED.generation);

ALTER TABLE spec_source_generations ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON spec_source_generations;
CREATE POLICY tenant_isolation ON spec_source_generations
    USING (tenant = current_setting('app.current_tenant', true));

ALTER TABLE spec_staging ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON spec_staging;
CREATE POLICY tenant_isolation ON spec_staging
    USING (tenant = current_setting('app.current_tenant', true));

ALTER TABLE tenant_constraint_generations ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON tenant_constraint_generations;
CREATE POLICY tenant_isolation ON tenant_constraint_generations
    USING (tenant = current_setting('app.current_tenant', true));
