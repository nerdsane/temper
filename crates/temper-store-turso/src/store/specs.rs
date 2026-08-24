//! Spec persistence: upsert, verification updates, and startup loading.

use std::collections::BTreeMap;

use libsql::{TransactionBehavior, params};
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{TursoEventStore, TursoInstalledAppRow, TursoSpecRow, write_gate::WritePriority};
use crate::TursoSpecVerificationUpdate;
use crate::metrics::TursoQueryTimer;

#[derive(Debug)]
struct ExistingSpecFingerprint {
    content_hash: Option<String>,
    csdl_xml: Option<String>,
    committed: bool,
}

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
        let _write_permit = self
            .acquire_write_permit("turso.upsert_spec", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        // When content_hash matches the existing row, keep verification intact.
        // Otherwise reset to pending so the cascade re-runs.
        conn.execute(
            "INSERT INTO specs (tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version, verified, verification_status, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 1, 0, 'pending', datetime('now'))
             ON CONFLICT (tenant, entity_type) DO UPDATE SET
                 ioa_source = CASE
                     WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                     THEN excluded.ioa_source ELSE specs.ioa_source END,
                 csdl_xml = CASE
                     WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                     THEN excluded.csdl_xml ELSE specs.csdl_xml END,
                 content_hash = CASE
                     WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                     THEN excluded.content_hash ELSE specs.content_hash END,
                 committed = CASE
                     WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                     THEN 0 ELSE specs.committed END,
                 version = CASE
                     WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                     THEN specs.version + 1 ELSE specs.version END,
                 verified = CASE
                     WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                     THEN 0 ELSE specs.verified END,
                 verification_status = CASE
                     WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                     THEN 'pending' ELSE specs.verification_status END,
                 levels_passed = CASE
                     WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                     THEN NULL ELSE specs.levels_passed END,
                 levels_total = CASE
                     WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                     THEN NULL ELSE specs.levels_total END,
                 verification_result = CASE
                     WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                     THEN NULL ELSE specs.verification_result END,
                 updated_at = CASE
                     WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                     THEN datetime('now') ELSE specs.updated_at END",
            params![tenant, entity_type, ioa_source, csdl_xml, content_hash],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Atomically upsert multiple specs, record the app installation, optionally
    /// write a Cedar policy, and mark all tenant specs as committed — all within
    /// a single libsql transaction.
    ///
    /// This eliminates the crash-vulnerability window where individual upserts
    /// leave specs with `committed=0` that get garbage-collected on restart.
    #[instrument(skip_all, fields(tenant, app_name, otel.name = "turso.upsert_specs_and_commit"))]
    pub async fn upsert_specs_and_commit(
        &self,
        tenant: &str,
        specs: &[(&str, &str, &str, &str)], // (entity_type, ioa_source, csdl_xml, content_hash)
        policy: Option<&str>,
        app_name: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.upsert_specs_and_commit");
        let conn = self.configured_connection().await?;
        let spec_indices = self
            .spec_indices_requiring_upsert(&conn, tenant, specs)
            .await?;
        let policy_needs_write = Self::tenant_policy_needs_write(&conn, tenant, policy).await?;
        let app_needs_write = Self::installed_app_needs_write(&conn, tenant, app_name).await?;

        if spec_indices.is_empty() && !policy_needs_write && !app_needs_write {
            return Ok(());
        }

        let _write_permit = self
            .acquire_write_permit("turso.upsert_specs_and_commit", WritePriority::High)
            .await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        for index in spec_indices {
            let (entity_type, ioa_source, csdl_xml, content_hash) = specs[index];
            tx.execute(
                "INSERT INTO specs (tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version, verified, verification_status, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, 1, 0, 'pending', datetime('now'))
                 ON CONFLICT (tenant, entity_type) DO UPDATE SET
                     ioa_source = CASE
                         WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                         THEN excluded.ioa_source ELSE specs.ioa_source END,
                     csdl_xml = CASE
                         WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                         THEN excluded.csdl_xml ELSE specs.csdl_xml END,
                     content_hash = CASE
                         WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                         THEN excluded.content_hash ELSE specs.content_hash END,
                     committed = 1,
                     version = CASE
                         WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                         THEN specs.version + 1 ELSE specs.version END,
                     verified = CASE
                         WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                         THEN 0 ELSE specs.verified END,
                     verification_status = CASE
                         WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                         THEN 'pending' ELSE specs.verification_status END,
                     levels_passed = CASE
                         WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                         THEN NULL ELSE specs.levels_passed END,
                     levels_total = CASE
                         WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                         THEN NULL ELSE specs.levels_total END,
                     verification_result = CASE
                         WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml
                         THEN NULL ELSE specs.verification_result END,
                     updated_at = CASE
                         WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml OR specs.committed != 1
                         THEN datetime('now') ELSE specs.updated_at END",
                params![tenant, entity_type, ioa_source, csdl_xml, content_hash],
            )
            .await
            .map_err(storage_error)?;
        }

        if policy_needs_write && let Some(policy_text) = policy {
            tx.execute(
                "INSERT INTO tenant_policies (tenant, policy_text, updated_at) \
                 VALUES (?1, ?2, datetime('now')) \
                 ON CONFLICT(tenant) DO UPDATE SET policy_text = ?2, updated_at = datetime('now')",
                params![tenant, policy_text],
            )
            .await
            .map_err(storage_error)?;
        }

        if app_needs_write {
            tx.execute(
                "INSERT OR IGNORE INTO tenant_installed_apps (tenant_id, app_name) VALUES (?1, ?2)",
                params![tenant, app_name],
            )
            .await
            .map_err(storage_error)?;
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    async fn spec_indices_requiring_upsert(
        &self,
        conn: &libsql::Connection,
        tenant: &str,
        specs: &[(&str, &str, &str, &str)],
    ) -> Result<Vec<usize>, PersistenceError> {
        if specs.is_empty() {
            return Ok(Vec::new());
        }

        let entity_types = specs
            .iter()
            .map(|(entity_type, _, _, _)| *entity_type)
            .collect::<Vec<_>>();
        let existing = Self::load_spec_fingerprints(conn, tenant, &entity_types).await?;
        Ok(specs
            .iter()
            .enumerate()
            .filter_map(
                |(index, (entity_type, _ioa_source, csdl_xml, content_hash))| {
                    let needs_upsert = existing.get(*entity_type).is_none_or(|fingerprint| {
                        fingerprint.content_hash.as_deref() != Some(*content_hash)
                            || fingerprint.csdl_xml.as_deref() != Some(*csdl_xml)
                            || !fingerprint.committed
                    });
                    needs_upsert.then_some(index)
                },
            )
            .collect())
    }

    async fn load_spec_fingerprints(
        conn: &libsql::Connection,
        tenant: &str,
        entity_types: &[&str],
    ) -> Result<BTreeMap<String, ExistingSpecFingerprint>, PersistenceError> {
        if entity_types.is_empty() {
            return Ok(BTreeMap::new());
        }

        let placeholders = (2..entity_types.len() + 2)
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT entity_type, content_hash, csdl_xml, committed \
             FROM specs \
             WHERE tenant = ?1 AND entity_type IN ({placeholders})"
        );
        let mut values: Vec<libsql::Value> = vec![tenant.to_string().into()];
        values.extend(
            entity_types
                .iter()
                .map(|entity_type| (*entity_type).to_string().into()),
        );

        let mut rows = conn
            .query(&sql, libsql::params_from_iter(values))
            .await
            .map_err(storage_error)?;
        let mut existing = BTreeMap::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_type: String = row.get(0).map_err(storage_error)?;
            let content_hash: Option<String> = row.get(1).map_err(storage_error)?;
            let csdl_xml: Option<String> = row.get(2).map_err(storage_error)?;
            let committed = row
                .get::<Option<i64>>(3)
                .map_err(storage_error)?
                .unwrap_or(1)
                != 0;
            existing.insert(
                entity_type,
                ExistingSpecFingerprint {
                    content_hash,
                    csdl_xml,
                    committed,
                },
            );
        }
        Ok(existing)
    }

    async fn tenant_policy_needs_write(
        conn: &libsql::Connection,
        tenant: &str,
        policy: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        let Some(policy_text) = policy else {
            return Ok(false);
        };

        let mut rows = conn
            .query(
                "SELECT policy_text FROM tenant_policies WHERE tenant = ?1 LIMIT 1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(true);
        };
        let existing: String = row.get(0).map_err(storage_error)?;
        Ok(existing != policy_text)
    }

    async fn installed_app_needs_write(
        conn: &libsql::Connection,
        tenant: &str,
        app_name: &str,
    ) -> Result<bool, PersistenceError> {
        let mut rows = conn
            .query(
                "SELECT 1 FROM tenant_installed_apps WHERE tenant_id = ?1 AND app_name = ?2 LIMIT 1",
                params![tenant, app_name],
            )
            .await
            .map_err(storage_error)?;
        Ok(rows.next().await.map_err(storage_error)?.is_none())
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
                 registry_url, registry_tenant, dependency_lock_digest, app_version, bundle_digest, spec_digest,
                 policy_digest, wasm_digest, content_digest, seed_digest,
                 installed_at, last_reconciled_at, status
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, datetime('now'), datetime('now'), ?20)
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
                 dependency_lock_digest = excluded.dependency_lock_digest,
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
                record.dependency_lock_digest.as_str(),
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
                        registry_url, registry_tenant, dependency_lock_digest, app_version, bundle_digest, spec_digest,
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
            dependency_lock_digest: row.get(11).map_err(storage_error)?,
            app_version: row.get(12).map_err(storage_error)?,
            bundle_digest: row.get(13).map_err(storage_error)?,
            spec_digest: row.get(14).map_err(storage_error)?,
            policy_digest: row.get(15).map_err(storage_error)?,
            wasm_digest: row.get(16).map_err(storage_error)?,
            content_digest: row.get(17).map_err(storage_error)?,
            seed_digest: row.get(18).map_err(storage_error)?,
            installed_at: row.get(19).map_err(storage_error)?,
            last_reconciled_at: row.get(20).map_err(storage_error)?,
            status: row.get(21).map_err(storage_error)?,
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
            "UPDATE specs SET committed = 1, updated_at = datetime('now') WHERE tenant = ?1 AND committed != 1",
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
