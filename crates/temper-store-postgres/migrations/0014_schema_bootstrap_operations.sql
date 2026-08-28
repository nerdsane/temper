-- ADR-0192: durable governed first-entity bootstrap coordinator.
CREATE TABLE IF NOT EXISTS schema_bootstrap_operations (
    tenant             TEXT  NOT NULL,
    caller_authority   TEXT  NOT NULL,
    idempotency_key    TEXT  NOT NULL,
    operation_json     JSONB NOT NULL,
    PRIMARY KEY (tenant, caller_authority, idempotency_key)
);

CREATE TABLE IF NOT EXISTS schema_bootstrap_targets (
    tenant                  TEXT NOT NULL,
    scope_kind              TEXT NOT NULL,
    scope_id                TEXT NOT NULL,
    bundle_digest           TEXT NOT NULL,
    entity_type             TEXT NOT NULL,
    entity_id               TEXT NOT NULL,
    owner_caller_authority  TEXT NOT NULL,
    owner_idempotency_key   TEXT NOT NULL,
    PRIMARY KEY (tenant, scope_kind, scope_id, bundle_digest, entity_type, entity_id)
);

ALTER TABLE schema_bootstrap_operations ENABLE ROW LEVEL SECURITY;
ALTER TABLE schema_bootstrap_targets ENABLE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS schema_bootstrap_operations_tenant_isolation ON schema_bootstrap_operations;
CREATE POLICY schema_bootstrap_operations_tenant_isolation ON schema_bootstrap_operations
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));

DROP POLICY IF EXISTS schema_bootstrap_targets_tenant_isolation ON schema_bootstrap_targets;
CREATE POLICY schema_bootstrap_targets_tenant_isolation ON schema_bootstrap_targets
    USING (tenant = current_setting('app.tenant_id', true))
    WITH CHECK (tenant = current_setting('app.tenant_id', true));
