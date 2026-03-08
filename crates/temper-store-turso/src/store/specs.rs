//! Spec persistence: upsert, verification updates, and startup loading.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{TursoEventStore, TursoSpecRow};
use crate::TursoSpecVerificationUpdate;

impl TursoEventStore {
    /// Upsert a spec source (IOA + CSDL) for a tenant/entity_type.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.upsert_spec"))]
    pub async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO specs (tenant, entity_type, ioa_source, csdl_xml, version, verified, verification_status, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, 0, 'pending', datetime('now'))
             ON CONFLICT (tenant, entity_type) DO UPDATE SET
                 ioa_source = excluded.ioa_source,
                 csdl_xml = excluded.csdl_xml,
                 version = specs.version + 1,
                 verified = 0,
                 verification_status = 'pending',
                 levels_passed = NULL,
                 levels_total = NULL,
                 verification_result = NULL,
                 updated_at = datetime('now')",
            params![tenant, entity_type, ioa_source, csdl_xml],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Persist verification result for a spec.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.persist_spec_verification"))]
    pub async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        update: TursoSpecVerificationUpdate<'_>,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "UPDATE specs SET
                 verified = ?3,
                 verification_status = ?4,
                 levels_passed = ?5,
                 levels_total = ?6,
                 verification_result = ?7,
                 updated_at = datetime('now')
             WHERE tenant = ?1 AND entity_type = ?2",
            params![
                tenant,
                entity_type,
                update.verified as i64,
                update.status,
                update.levels_passed,
                update.levels_total,
                update.verification_result_json
            ],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load all persisted specs (for startup recovery).
    #[instrument(skip_all, fields(otel.name = "turso.load_specs"))]
    pub async fn load_specs(&self) -> Result<Vec<TursoSpecRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, \
                        levels_passed, levels_total, verification_result, updated_at \
                 FROM specs \
                 ORDER BY tenant, entity_type",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TursoSpecRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                entity_type: row.get::<String>(1).map_err(storage_error)?,
                ioa_source: row.get::<String>(2).map_err(storage_error)?,
                csdl_xml: row.get::<Option<String>>(3).map_err(storage_error)?,
                verification_status: row.get::<String>(4).map_err(storage_error)?,
                verified: row.get::<i64>(5).map_err(storage_error)? != 0,
                levels_passed: row
                    .get::<Option<i64>>(6)
                    .map_err(storage_error)?
                    .map(|v| v as i32),
                levels_total: row
                    .get::<Option<i64>>(7)
                    .map_err(storage_error)?
                    .map(|v| v as i32),
                verification_result: row.get::<Option<String>>(8).map_err(storage_error)?,
                updated_at: row.get::<String>(9).map_err(storage_error)?,
            });
        }
        Ok(out)
    }
}
