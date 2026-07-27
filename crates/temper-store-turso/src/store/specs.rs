//! Spec persistence: upsert, verification updates, and startup loading.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{TursoEventStore, TursoInstalledAppRow, TursoSpecRow};
use crate::TursoSpecVerificationUpdate;
use crate::metrics::TursoQueryTimer;

impl TursoEventStore {
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
             WHERE tenant = ?1 AND entity_type = ?2
               AND (
                   verified IS NOT ?3
                   OR verification_status IS NOT ?4
                   OR levels_passed IS NOT ?5
                   OR levels_total IS NOT ?6
                   OR CASE
                       WHEN verification_result IS NULL AND ?7 IS NULL THEN 0
                       WHEN verification_result IS NULL OR ?7 IS NULL THEN 1
                       WHEN json_valid(verification_result) != 0 AND json_valid(?7) != 0
                       THEN json_remove(verification_result, '$.verified_at') IS NOT json_remove(?7, '$.verified_at')
                       ELSE verification_result IS NOT ?7
                   END
               )",
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
        conn.execute(
            "UPDATE spec_staging SET
                 verified = ?3,
                 verification_status = ?4,
                 levels_passed = ?5,
                 levels_total = ?6,
                 verification_result = ?7,
                 staged_at = datetime('now')
             WHERE tenant = ?1 AND entity_type = ?2
               AND (
                   verified IS NOT ?3
                   OR verification_status IS NOT ?4
                   OR levels_passed IS NOT ?5
                   OR levels_total IS NOT ?6
                   OR CASE
                       WHEN verification_result IS NULL AND ?7 IS NULL THEN 0
                       WHEN verification_result IS NULL OR ?7 IS NULL THEN 1
                       WHEN json_valid(verification_result) != 0 AND json_valid(?7) != 0
                       THEN json_remove(verification_result, '$.verified_at') IS NOT json_remove(?7, '$.verified_at')
                       ELSE verification_result IS NOT ?7
                   END
               )",
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
                "SELECT entity_type, content_hash, verified FROM specs WHERE tenant = ?1 AND committed = 1",
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

    /// Record or update digest metadata for an installed OS app.
    #[instrument(skip_all, fields(tenant_id = %record.tenant_id, app_name = %record.app_name, otel.name = "turso.record_installed_app_metadata"))]
    pub async fn record_installed_app_metadata(
        &self,
        record: &TursoInstalledAppRow,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.record_installed_app_metadata");
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO tenant_installed_apps (
                 tenant_id, app_name, source_kind, app_ref, version_hash,
                 pinned_version_hash, current_version_hash, follow_policy, closure_id,
                 registry_url, registry_tenant, app_version, bundle_digest, spec_digest,
                 policy_digest, wasm_digest, content_digest, seed_digest,
                 installed_at, last_reconciled_at, status
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, datetime('now'), datetime('now'), ?19)
             ON CONFLICT(tenant_id, app_name) DO UPDATE SET
                 source_kind = excluded.source_kind,
                 app_ref = excluded.app_ref,
                 version_hash = excluded.version_hash,
                 pinned_version_hash = excluded.pinned_version_hash,
                 current_version_hash = excluded.current_version_hash,
                 follow_policy = excluded.follow_policy,
                 closure_id = excluded.closure_id,
                 registry_url = excluded.registry_url,
                 registry_tenant = excluded.registry_tenant,
                 app_version = excluded.app_version,
                 bundle_digest = excluded.bundle_digest,
                 spec_digest = excluded.spec_digest,
                 policy_digest = excluded.policy_digest,
                 wasm_digest = excluded.wasm_digest,
                 content_digest = excluded.content_digest,
                 seed_digest = excluded.seed_digest,
                 last_reconciled_at = datetime('now'),
                 status = excluded.status",
            params![
                record.tenant_id.as_str(),
                record.app_name.as_str(),
                record.source_kind.as_str(),
                record.app_ref.as_str(),
                record.version_hash.as_str(),
                record.pinned_version_hash.as_str(),
                record.current_version_hash.as_str(),
                record.follow_policy.as_str(),
                record.closure_id.as_str(),
                record.registry_url.as_str(),
                record.registry_tenant.as_str(),
                record.app_version.as_str(),
                record.bundle_digest.as_str(),
                record.spec_digest.as_str(),
                record.policy_digest.as_str(),
                record.wasm_digest.as_str(),
                record.content_digest.as_str(),
                record.seed_digest.as_str(),
                record.status.as_str()
            ],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load digest metadata for an installed OS app.
    #[instrument(skip_all, fields(tenant_id, app_name, otel.name = "turso.get_installed_app"))]
    pub async fn get_installed_app(
        &self,
        tenant_id: &str,
        app_name: &str,
    ) -> Result<Option<TursoInstalledAppRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.get_installed_app");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant_id, app_name, source_kind, app_ref, version_hash,
                        pinned_version_hash, current_version_hash, follow_policy, closure_id,
                        registry_url, registry_tenant, app_version, bundle_digest, spec_digest,
                        policy_digest, wasm_digest, content_digest, seed_digest,
                        installed_at, last_reconciled_at, status
                 FROM tenant_installed_apps
                 WHERE tenant_id = ?1 AND app_name = ?2
                 LIMIT 1",
                params![tenant_id, app_name],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };

        Ok(Some(TursoInstalledAppRow {
            tenant_id: row.get(0).map_err(storage_error)?,
            app_name: row.get(1).map_err(storage_error)?,
            source_kind: row.get(2).map_err(storage_error)?,
            app_ref: row.get(3).map_err(storage_error)?,
            version_hash: row.get(4).map_err(storage_error)?,
            pinned_version_hash: row.get(5).map_err(storage_error)?,
            current_version_hash: row.get(6).map_err(storage_error)?,
            follow_policy: row.get(7).map_err(storage_error)?,
            closure_id: row.get(8).map_err(storage_error)?,
            registry_url: row.get(9).map_err(storage_error)?,
            registry_tenant: row.get(10).map_err(storage_error)?,
            app_version: row.get(11).map_err(storage_error)?,
            bundle_digest: row.get(12).map_err(storage_error)?,
            spec_digest: row.get(13).map_err(storage_error)?,
            policy_digest: row.get(14).map_err(storage_error)?,
            wasm_digest: row.get(15).map_err(storage_error)?,
            content_digest: row.get(16).map_err(storage_error)?,
            seed_digest: row.get(17).map_err(storage_error)?,
            installed_at: row.get(18).map_err(storage_error)?,
            last_reconciled_at: row.get(19).map_err(storage_error)?,
            status: row.get(20).map_err(storage_error)?,
        }))
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
                        levels_passed, levels_total, verification_result, content_hash, version, updated_at, committed \
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
                version: row.get::<i64>(10).map_err(storage_error)?,
                updated_at: row.get::<String>(11).map_err(storage_error)?,
                committed: row
                    .get::<Option<i64>>(12)
                    .map_err(storage_error)?
                    .unwrap_or(1)
                    != 0,
            });
        }
        Ok(out)
    }
}
