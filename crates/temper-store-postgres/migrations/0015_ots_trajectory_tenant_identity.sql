-- Tenant-scoped OTS trajectory identity.
--
-- The table was keyed on trajectory_id alone. That id is chosen by the
-- uploading harness and one database holds every tenant's rows, so two tenants
-- could collide on one id — and the upsert resolved the collision by rewriting
-- the existing row, tenant column included, handing one tenant's trajectory to
-- another. Reads were already tenant-scoped; the identity has to be too.
--
-- No row can violate the new key: the old one was global, so (tenant,
-- trajectory_id) is unique wherever trajectory_id was.

ALTER TABLE ots_trajectories
    DROP CONSTRAINT IF EXISTS ots_trajectories_pkey;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'ots_trajectories_tenant_identity'
          AND conrelid = 'ots_trajectories'::regclass
    ) THEN
        ALTER TABLE ots_trajectories
            ADD CONSTRAINT ots_trajectories_tenant_identity
            PRIMARY KEY (tenant, trajectory_id);
    END IF;
END
$$;
