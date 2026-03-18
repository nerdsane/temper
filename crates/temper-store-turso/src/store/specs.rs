//! Spec persistence: upsert, verification updates, and startup loading.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{TursoEventStore, TursoSpecRow};
use crate::TursoSpecVerificationUpdate;
use crate::metrics::TursoQueryTimer;

impl TursoEventStore {
    /// Upsert a spec source (IOA + CSDL) for a tenant/entity_type.
    ///
    /// Uses content-hash gating: if the spec already exists with the same
    /// `content_hash` and is verified, verification status is preserved.
    /// Only resets to "pending" when the content actually changed.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.upsert_spec"))]
    pub async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.upsert_spec");
        let conn = self.configured_connection().await?;
        // When content_hash matches the existing row, keep verification intact.
        // Otherwise reset to pending so the cascade re-runs.
        conn.execute(
            "INSERT INTO specs (tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version, verified, verification_status, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 1, 0, 'pending', datetime('now'))
             ON CONFLICT (tenant, entity_type) DO UPDATE SET
                 ioa_source = excluded.ioa_source,
                 csdl_xml = excluded.csdl_xml,
                 content_hash = excluded.content_hash,
                 committed = 0,
                 version = specs.version + 1,
                 verified = CASE WHEN specs.content_hash = excluded.content_hash THEN specs.verified ELSE 0 END,
                 verification_status = CASE WHEN specs.content_hash = excluded.content_hash THEN specs.verification_status ELSE 'pending' END,
                 levels_passed = CASE WHEN specs.content_hash = excluded.content_hash THEN specs.levels_passed ELSE NULL END,
                 levels_total = CASE WHEN specs.content_hash = excluded.content_hash THEN specs.levels_total ELSE NULL END,
                 verification_result = CASE WHEN specs.content_hash = excluded.content_hash THEN specs.verification_result ELSE NULL END,
                 updated_at = datetime('now')",
            params![tenant, entity_type, ioa_source, csdl_xml, content_hash],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Delete a spec for a given tenant/entity_type.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.delete_spec"))]
    pub async fn delete_spec(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.delete_spec");
        let conn = self.configured_connection().await?;
        conn.execute(
            "DELETE FROM specs WHERE tenant = ?1 AND entity_type = ?2",
            params![tenant, entity_type],
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
        let _query_timer = TursoQueryTimer::start("turso.persist_spec_verification");
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

    /// Load verification cache: (entity_type → (content_hash, verified)) for a tenant.
    ///
    /// Used by bootstrap to skip the verification cascade when the spec
    /// content hasn't changed since the last successful verification.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.load_verification_cache"))]
    pub async fn load_verification_cache(
        &self,
        tenant: &str,
    ) -> Result<std::collections::BTreeMap<String, (String, bool)>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_verification_cache");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_type, content_hash, verified FROM specs WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let mut cache = std::collections::BTreeMap::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_type: String = row.get(0).map_err(storage_error)?;
            let hash: Option<String> = row.get(1).map_err(storage_error)?;
            let verified: i64 = row.get(2).map_err(storage_error)?;
            if let Some(h) = hash {
                cache.insert(entity_type, (h, verified != 0));
            }
        }
        Ok(cache)
    }

    // ── Installed Apps ─────────────────────────────────────────────

    /// Check if an OS app is already installed for a tenant.
    #[instrument(skip_all, fields(tenant_id, app_name, otel.name = "turso.is_app_installed"))]
    pub async fn is_app_installed(
        &self,
        tenant_id: &str,
        app_name: &str,
    ) -> Result<bool, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.is_app_installed");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT 1 FROM tenant_installed_apps WHERE tenant_id = ?1 AND app_name = ?2 LIMIT 1",
                params![tenant_id, app_name],
            )
            .await
            .map_err(storage_error)?;
        Ok(rows.next().await.map_err(storage_error)?.is_some())
    }

    /// Record that an OS app was installed in a tenant.
    #[instrument(skip_all, fields(tenant_id, app_name, otel.name = "turso.record_installed_app"))]
    pub async fn record_installed_app(
        &self,
        tenant_id: &str,
        app_name: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.record_installed_app");
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT OR IGNORE INTO tenant_installed_apps (tenant_id, app_name) VALUES (?1, ?2)",
            params![tenant_id, app_name],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// List all installed apps across all tenants (for boot + UI).
    #[instrument(skip_all, fields(otel.name = "turso.list_all_installed_apps"))]
    pub async fn list_all_installed_apps(&self) -> Result<Vec<(String, String)>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.list_all_installed_apps");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant_id, app_name FROM tenant_installed_apps ORDER BY tenant_id, app_name",
                (),
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push((
                row.get::<String>(0).map_err(storage_error)?,
                row.get::<String>(1).map_err(storage_error)?,
            ));
        }
        Ok(out)
    }

    /// Remove all installed app records for a tenant (for deletion cleanup).
    #[instrument(skip_all, fields(tenant_id, otel.name = "turso.remove_installed_apps"))]
    pub async fn remove_installed_apps(&self, tenant_id: &str) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.remove_installed_apps");
        let conn = self.configured_connection().await?;
        conn.execute(
            "DELETE FROM tenant_installed_apps WHERE tenant_id = ?1",
            params![tenant_id],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    // ── Spec Loading ──────────────────────────────────────────────

    /// Load all persisted specs (for startup recovery).
    #[instrument(skip_all, fields(otel.name = "turso.load_specs"))]
    pub async fn load_specs(&self) -> Result<Vec<TursoSpecRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_specs");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, \
                        levels_passed, levels_total, verification_result, content_hash, updated_at, committed \
                 FROM specs \
                 WHERE committed = 1 \
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
                content_hash: row.get::<Option<String>>(9).map_err(storage_error)?,
                updated_at: row.get::<String>(10).map_err(storage_error)?,
                committed: row
                    .get::<Option<i64>>(11)
                    .map_err(storage_error)?
                    .unwrap_or(1)
                    != 0,
            });
        }
        Ok(out)
    }

    /// Mark all uncommitted specs for a tenant as committed.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.commit_specs"))]
    pub async fn commit_specs(&self, tenant: &str) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.commit_specs");
        let conn = self.configured_connection().await?;
        conn.execute(
            "UPDATE specs SET committed = 1, updated_at = datetime('now') WHERE tenant = ?1",
            params![tenant],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Delete all uncommitted specs across all tenants.
    #[instrument(skip_all, fields(otel.name = "turso.delete_uncommitted_specs"))]
    pub async fn delete_uncommitted_specs(&self) -> Result<usize, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.delete_uncommitted_specs");
        let conn = self.configured_connection().await?;
        let affected = conn
            .execute("DELETE FROM specs WHERE committed = 0", ())
            .await
            .map_err(storage_error)?;
        Ok(affected as usize)
    }
}
