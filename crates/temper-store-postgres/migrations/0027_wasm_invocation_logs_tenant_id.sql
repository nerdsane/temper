-- A tenant's invocation log in id order, so trimming it to its newest rows
-- reads only that tenant's rows. With only the tenant index, a prepared trim
-- (the generic plan) walked the primary key back through every tenant's rows
-- to find one tenant's cut-off: 30 ms at 40 tenants of 10,000 rows, growing
-- with the whole table. This index also serves every tenant-equality read,
-- so it replaces the tenant-only index, and the previous release's queries
-- use it the same way.
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_tenant_id ON wasm_invocation_logs (tenant, id);

DROP INDEX IF EXISTS idx_wasm_invocation_logs_tenant;
