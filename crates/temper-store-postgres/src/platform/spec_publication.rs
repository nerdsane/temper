//! Atomic PostgreSQL tenant spec publication.

use super::*;

impl PostgresEventStore {
    /// Atomically publish changed specs and remove replace-mode omissions.
    #[expect(
        clippy::too_many_arguments,
        reason = "atomic publication boundary mirrors the platform-store contract"
    )]
    pub async fn publish_specs(
        &self,
        tenant: &str,
        specs: &[(&str, &str, &str, &str)],
        replace: bool,
        constraints: Option<Option<&str>>,
        policy: Option<&str>,
        os_app: Option<&PostgresInstalledAppRow>,
        wasm_modules: &[(&str, &[u8], &str, &str)],
        policy_owner: Option<&str>,
        policy_entries: &[(&str, &str, &str)],
    ) -> Result<Vec<String>, PersistenceError> {
        let mut seen_types = std::collections::BTreeSet::new();
        for (entity_type, _, _, _) in specs {
            if !seen_types.insert(*entity_type) {
                return Err(PersistenceError::Storage(format!(
                    "duplicate entity type {entity_type} in spec publication for tenant {tenant}"
                )));
            }
        }
        let mut seen_modules = std::collections::BTreeSet::new();
        for (module_name, _, _, _) in wasm_modules {
            if !seen_modules.insert(*module_name) {
                return Err(PersistenceError::Storage(format!(
                    "duplicate WASM module {module_name} in spec publication for tenant {tenant}"
                )));
            }
        }
        let mut seen_policy_ids = std::collections::BTreeSet::new();
        for (policy_id, _, _) in policy_entries {
            if !seen_policy_ids.insert(*policy_id) {
                return Err(PersistenceError::Storage(format!(
                    "duplicate Cedar policy {policy_id} in spec publication for tenant {tenant}"
                )));
            }
        }
        let mut tx = self.pool().begin().await.map_err(storage_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("spec-publication:{tenant}"))
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        let incoming_types = specs
            .iter()
            .map(|(entity_type, _, _, _)| *entity_type)
            .collect::<std::collections::BTreeSet<_>>();
        let removed_entity_types = if replace {
            crate::dbm::postgres_query_scalar!(
                "SELECT entity_type FROM specs WHERE tenant = $1 FOR UPDATE",
            )
            .bind(tenant)
            .fetch_all(&mut *tx)
            .await
            .map_err(storage_error)?
            .into_iter()
            .filter(|entity_type: &String| !incoming_types.contains(entity_type.as_str()))
            .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for (entity_type, ioa_source, csdl_xml, content_hash) in specs {
            crate::dbm::postgres_query!(
                "INSERT INTO specs \
                 (tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version, verified, verification_status, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, true, 1, false, 'pending', now()) \
                 ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                     ioa_source = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.ioa_source ELSE specs.ioa_source END, \
                     csdl_xml = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.csdl_xml ELSE specs.csdl_xml END, \
                     content_hash = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.content_hash ELSE specs.content_hash END, \
                     committed = true, \
                     version = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN specs.version + 1 ELSE specs.version END, \
                     verified = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN false ELSE specs.verified END, \
                     verification_status = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN 'pending' ELSE specs.verification_status END, \
                     levels_passed = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.levels_passed END, \
                     levels_total = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.levels_total END, \
                     verification_result = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.verification_result END, \
                     updated_at = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml OR specs.committed = false THEN now() ELSE specs.updated_at END",
            )
            .bind(tenant)
            .bind(*entity_type)
            .bind(*ioa_source)
            .bind(*csdl_xml)
            .bind(*content_hash)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        }
        for entity_type in &removed_entity_types {
            crate::dbm::postgres_query!(
                "DELETE FROM specs WHERE tenant = $1 AND entity_type = $2",
            )
            .bind(tenant)
            .bind(entity_type)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        }
        match constraints {
            None => {}
            Some(Some(source)) => {
                crate::dbm::postgres_query!(
                    "INSERT INTO tenant_constraints \
                     (tenant, cross_invariants_toml, version, updated_at) \
                     VALUES ($1, $2, 1, now()) \
                     ON CONFLICT (tenant) DO UPDATE SET \
                         cross_invariants_toml = EXCLUDED.cross_invariants_toml, \
                         version = tenant_constraints.version + 1, updated_at = now()",
                )
                .bind(tenant)
                .bind(source)
                .execute(&mut *tx)
                .await
                .map_err(storage_error)?;
            }
            Some(None) => {
                crate::dbm::postgres_query!("DELETE FROM tenant_constraints WHERE tenant = $1",)
                    .bind(tenant)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            }
        }
        if let Some(policy) = policy {
            crate::dbm::postgres_query!(
                "INSERT INTO tenant_policies (tenant, policy_text, updated_at) \
                 VALUES ($1, $2, now()) \
                 ON CONFLICT (tenant) DO UPDATE SET \
                     policy_text = EXCLUDED.policy_text, updated_at = now()",
            )
            .bind(tenant)
            .bind(policy)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        }
        if let Some(owner) = policy_owner {
            crate::dbm::postgres_query!(
                "DELETE FROM policies WHERE tenant = $1 AND created_by = $2",
            )
            .bind(tenant)
            .bind(owner)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
            for (policy_id, cedar_text, created_by) in policy_entries {
                let policy_hash = compute_policy_hash(cedar_text);
                crate::dbm::postgres_query!(
                    "INSERT INTO policies \
                     (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
                     VALUES ($1, $2, $3, $4, now(), $5, true) \
                     ON CONFLICT (tenant, policy_id) DO UPDATE SET \
                         cedar_text = EXCLUDED.cedar_text, policy_hash = EXCLUDED.policy_hash, \
                         created_by = EXCLUDED.created_by, created_at = now(), enabled = true",
                )
                .bind(tenant)
                .bind(*policy_id)
                .bind(*cedar_text)
                .bind(policy_hash)
                .bind(*created_by)
                .execute(&mut *tx)
                .await
                .map_err(storage_error)?;
            }
        }
        for (module_name, wasm_bytes, sha256_hash, source) in wasm_modules {
            let replace_uploaded_wasm = *source == BUNDLED_REPLACE_UPLOAD_SOURCE;
            let persisted_source = if replace_uploaded_wasm {
                "bundled"
            } else {
                *source
            };
            crate::dbm::postgres_query!(
                "INSERT INTO wasm_modules \
                 (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at, source) \
                 VALUES ($1, $2, $3, $4, 1, $5, now(), $6) \
                 ON CONFLICT (tenant, module_name) DO UPDATE SET \
                     wasm_bytes = EXCLUDED.wasm_bytes, sha256_hash = EXCLUDED.sha256_hash, \
                     version = wasm_modules.version + 1, size_bytes = EXCLUDED.size_bytes, \
                     updated_at = now(), source = EXCLUDED.source \
                 WHERE wasm_modules.sha256_hash IS DISTINCT FROM EXCLUDED.sha256_hash \
                    AND ($7 OR EXCLUDED.source = 'upload' OR wasm_modules.source = 'bundled')",
            )
            .bind(tenant)
            .bind(*module_name)
            .bind(*wasm_bytes)
            .bind(*sha256_hash)
            .bind(wasm_bytes.len() as i32)
            .bind(persisted_source)
            .bind(replace_uploaded_wasm)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        }
        if let Some(record) = os_app {
            crate::dbm::postgres_query!(
                "INSERT INTO tenant_installed_apps \
                 (tenant, app_name, source_kind, app_ref, version_hash, pinned_version_hash, current_version_hash, follow_policy, closure_id, registry_url, registry_tenant, \
                  app_version, bundle_digest, spec_digest, policy_digest, wasm_digest, content_digest, seed_digest, installed_at, last_reconciled_at, status) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, now(), now(), $19) \
                 ON CONFLICT (tenant, app_name) DO UPDATE SET \
                     source_kind = EXCLUDED.source_kind, app_ref = EXCLUDED.app_ref, \
                     version_hash = EXCLUDED.version_hash, pinned_version_hash = EXCLUDED.pinned_version_hash, \
                     current_version_hash = EXCLUDED.current_version_hash, follow_policy = EXCLUDED.follow_policy, \
                     closure_id = EXCLUDED.closure_id, registry_url = EXCLUDED.registry_url, \
                     registry_tenant = EXCLUDED.registry_tenant, app_version = EXCLUDED.app_version, \
                     bundle_digest = EXCLUDED.bundle_digest, spec_digest = EXCLUDED.spec_digest, \
                     policy_digest = EXCLUDED.policy_digest, wasm_digest = EXCLUDED.wasm_digest, \
                     content_digest = EXCLUDED.content_digest, seed_digest = EXCLUDED.seed_digest, \
                     last_reconciled_at = now(), status = EXCLUDED.status",
            )
            .bind(tenant)
            .bind(&record.app_name)
            .bind(&record.source_kind)
            .bind(&record.app_ref)
            .bind(&record.version_hash)
            .bind(&record.pinned_version_hash)
            .bind(&record.current_version_hash)
            .bind(&record.follow_policy)
            .bind(&record.closure_id)
            .bind(&record.registry_url)
            .bind(&record.registry_tenant)
            .bind(&record.app_version)
            .bind(&record.bundle_digest)
            .bind(&record.spec_digest)
            .bind(&record.policy_digest)
            .bind(&record.wasm_digest)
            .bind(&record.content_digest)
            .bind(&record.seed_digest)
            .bind(&record.status)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(removed_entity_types)
    }
}
