-- ADR-0159: immutable task-scoped schema deployment authority.
CREATE TABLE IF NOT EXISTS schema_deployments (
    tenant        TEXT  NOT NULL,
    scope_kind    TEXT  NOT NULL,
    scope_id      TEXT  NOT NULL,
    bundle_digest TEXT  NOT NULL,
    record_json   JSONB NOT NULL,
    PRIMARY KEY (tenant, scope_kind, scope_id, bundle_digest)
);

CREATE TABLE IF NOT EXISTS schema_deployment_idempotency (
    tenant         TEXT NOT NULL,
    operation      TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    bundle_digest  TEXT NOT NULL,
    scope_kind     TEXT NOT NULL,
    scope_id       TEXT NOT NULL,
    PRIMARY KEY (tenant, operation, idempotency_key)
);

CREATE TABLE IF NOT EXISTS schema_verification_receipts (
    tenant        TEXT  NOT NULL,
    scope_kind    TEXT  NOT NULL,
    scope_id      TEXT  NOT NULL,
    bundle_digest TEXT  NOT NULL,
    receipt_id    TEXT  NOT NULL,
    receipt_json  JSONB NOT NULL,
    PRIMARY KEY (tenant, scope_kind, scope_id, bundle_digest, receipt_id)
);

CREATE TABLE IF NOT EXISTS schema_active_pointers (
    tenant       TEXT  NOT NULL,
    scope_kind   TEXT  NOT NULL,
    scope_id     TEXT  NOT NULL,
    pointer_json JSONB NOT NULL,
    PRIMARY KEY (tenant, scope_kind, scope_id)
);

CREATE TABLE IF NOT EXISTS schema_migration_jobs (
    tenant     TEXT  NOT NULL,
    job_id     TEXT  NOT NULL,
    scope_kind TEXT  NOT NULL,
    scope_id   TEXT  NOT NULL,
    job_json   JSONB NOT NULL,
    PRIMARY KEY (tenant, job_id)
);

CREATE TABLE IF NOT EXISTS schema_migration_idempotency (
    tenant          TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_digest  TEXT NOT NULL,
    job_id          TEXT NOT NULL,
    PRIMARY KEY (tenant, idempotency_key)
);

CREATE TABLE IF NOT EXISTS schema_migration_retry_idempotency (
    tenant            TEXT   NOT NULL,
    idempotency_key   TEXT   NOT NULL,
    request_digest    TEXT   NOT NULL,
    job_id            TEXT   NOT NULL,
    starting_sequence BIGINT NOT NULL CHECK (starting_sequence >= 0),
    request_id        TEXT   NOT NULL,
    PRIMARY KEY (tenant, idempotency_key)
);

CREATE TABLE IF NOT EXISTS schema_migration_shadow (
    tenant      TEXT  NOT NULL,
    job_id      TEXT  NOT NULL,
    entity_type TEXT  NOT NULL,
    entity_id   TEXT  NOT NULL,
    row_json    JSONB NOT NULL,
    PRIMARY KEY (tenant, job_id, entity_type, entity_id)
);

CREATE TABLE IF NOT EXISTS schema_migration_batch_receipts (
    tenant       TEXT  NOT NULL,
    job_id       TEXT  NOT NULL,
    receipt_id   TEXT  NOT NULL,
    receipt_json JSONB NOT NULL,
    PRIMARY KEY (tenant, job_id, receipt_id)
);

CREATE TABLE IF NOT EXISTS schema_migration_validation_receipts (
    tenant       TEXT  NOT NULL,
    job_id       TEXT  NOT NULL,
    receipt_id   TEXT  NOT NULL,
    receipt_json JSONB NOT NULL,
    PRIMARY KEY (tenant, job_id, receipt_id)
);

ALTER TABLE schema_deployments ENABLE ROW LEVEL SECURITY;
ALTER TABLE schema_deployment_idempotency ENABLE ROW LEVEL SECURITY;
ALTER TABLE schema_verification_receipts ENABLE ROW LEVEL SECURITY;
ALTER TABLE schema_active_pointers ENABLE ROW LEVEL SECURITY;
ALTER TABLE schema_migration_jobs ENABLE ROW LEVEL SECURITY;
ALTER TABLE schema_migration_idempotency ENABLE ROW LEVEL SECURITY;
ALTER TABLE schema_migration_retry_idempotency ENABLE ROW LEVEL SECURITY;
ALTER TABLE schema_migration_shadow ENABLE ROW LEVEL SECURITY;
ALTER TABLE schema_migration_batch_receipts ENABLE ROW LEVEL SECURITY;
ALTER TABLE schema_migration_validation_receipts ENABLE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS schema_deployments_tenant_isolation ON schema_deployments;
CREATE POLICY schema_deployments_tenant_isolation ON schema_deployments
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));
DROP POLICY IF EXISTS schema_deployment_idempotency_tenant_isolation ON schema_deployment_idempotency;
CREATE POLICY schema_deployment_idempotency_tenant_isolation ON schema_deployment_idempotency
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));
DROP POLICY IF EXISTS schema_verification_receipts_tenant_isolation ON schema_verification_receipts;
CREATE POLICY schema_verification_receipts_tenant_isolation ON schema_verification_receipts
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));
DROP POLICY IF EXISTS schema_active_pointers_tenant_isolation ON schema_active_pointers;
CREATE POLICY schema_active_pointers_tenant_isolation ON schema_active_pointers
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));
DROP POLICY IF EXISTS schema_migration_jobs_tenant_isolation ON schema_migration_jobs;
CREATE POLICY schema_migration_jobs_tenant_isolation ON schema_migration_jobs
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));
DROP POLICY IF EXISTS schema_migration_idempotency_tenant_isolation ON schema_migration_idempotency;
CREATE POLICY schema_migration_idempotency_tenant_isolation ON schema_migration_idempotency
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));
DROP POLICY IF EXISTS schema_migration_retry_idempotency_tenant_isolation ON schema_migration_retry_idempotency;
CREATE POLICY schema_migration_retry_idempotency_tenant_isolation ON schema_migration_retry_idempotency
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));
DROP POLICY IF EXISTS schema_migration_shadow_tenant_isolation ON schema_migration_shadow;
CREATE POLICY schema_migration_shadow_tenant_isolation ON schema_migration_shadow
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));
DROP POLICY IF EXISTS schema_migration_batch_receipts_tenant_isolation ON schema_migration_batch_receipts;
CREATE POLICY schema_migration_batch_receipts_tenant_isolation ON schema_migration_batch_receipts
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));
DROP POLICY IF EXISTS schema_migration_validation_receipts_tenant_isolation ON schema_migration_validation_receipts;
CREATE POLICY schema_migration_validation_receipts_tenant_isolation ON schema_migration_validation_receipts
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));
