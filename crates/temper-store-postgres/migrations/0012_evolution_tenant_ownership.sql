-- Evolution data is tenant-owned at the storage boundary.  Existing rows
-- predate explicit ownership and remain assigned to the historical default
-- tenant; every new read and write supplies a tenant predicate.
ALTER TABLE feature_requests
    ADD COLUMN IF NOT EXISTS tenant TEXT NOT NULL DEFAULT 'default';

ALTER TABLE evolution_records
    ADD COLUMN IF NOT EXISTS tenant TEXT NOT NULL DEFAULT 'default';

CREATE INDEX IF NOT EXISTS idx_feature_requests_tenant_disposition
    ON feature_requests (tenant, disposition, frequency DESC, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_evolution_records_tenant_type_status
    ON evolution_records (tenant, record_type, status, timestamp DESC);

CREATE INDEX IF NOT EXISTS idx_evolution_records_tenant_parent
    ON evolution_records (tenant, derived_from);
