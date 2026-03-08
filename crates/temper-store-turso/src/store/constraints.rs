//! Tenant-level cross-entity constraint persistence.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{TursoEventStore, TursoTenantConstraintRow};

impl TursoEventStore {
    /// Upsert tenant-level cross-entity constraint definitions.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.upsert_tenant_constraints"))]
    pub async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO tenant_constraints (tenant, cross_invariants_toml, version, updated_at)
             VALUES (?1, ?2, 1, datetime('now'))
             ON CONFLICT (tenant) DO UPDATE SET
                 cross_invariants_toml = excluded.cross_invariants_toml,
                 version = tenant_constraints.version + 1,
                 updated_at = datetime('now')",
            params![tenant, cross_invariants_toml],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Delete tenant-level cross-entity constraint definitions.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.delete_tenant_constraints"))]
    pub async fn delete_tenant_constraints(&self, tenant: &str) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "DELETE FROM tenant_constraints WHERE tenant = ?1",
            params![tenant],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load all tenant-level cross-entity constraint definitions.
    #[instrument(skip_all, fields(otel.name = "turso.load_tenant_constraints"))]
    pub async fn load_tenant_constraints(
        &self,
    ) -> Result<Vec<TursoTenantConstraintRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, cross_invariants_toml, version, updated_at \
                 FROM tenant_constraints \
                 ORDER BY tenant",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TursoTenantConstraintRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                cross_invariants_toml: row.get::<String>(1).map_err(storage_error)?,
                version: row.get::<i64>(2).map_err(storage_error)? as i32,
                updated_at: row.get::<String>(3).map_err(storage_error)?,
            });
        }
        Ok(out)
    }
}
