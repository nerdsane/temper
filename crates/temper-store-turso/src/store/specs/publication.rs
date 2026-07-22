//! Atomic durable publication for tenant spec generations.

use sha2::{Digest, Sha256};

use super::*;

impl TursoEventStore {
    /// Atomically publish changed specs and delete replace-mode omissions.
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
        os_app: Option<&TursoInstalledAppRow>,
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
        let conn = self.configured_connection().await?;
        let _write_permit = self
            .acquire_write_permit("turso.publish_specs", WritePriority::High)
            .await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let incoming_types = specs
            .iter()
            .map(|(entity_type, _, _, _)| *entity_type)
            .collect::<std::collections::BTreeSet<_>>();
        let removed_entity_types = if replace {
            let mut rows = tx
                .query(
                    "SELECT entity_type FROM specs WHERE tenant = ?1",
                    params![tenant],
                )
                .await
                .map_err(storage_error)?;
            let mut removed = Vec::new();
            while let Some(row) = rows.next().await.map_err(storage_error)? {
                let entity_type = row.get::<String>(0).map_err(storage_error)?;
                if !incoming_types.contains(entity_type.as_str()) {
                    removed.push(entity_type);
                }
            }
            removed
        } else {
            Vec::new()
        };
        for (entity_type, ioa_source, csdl_xml, content_hash) in specs {
            tx.execute(
                "INSERT INTO specs (tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version, verified, verification_status, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, 1, 0, 'pending', datetime('now'))
                 ON CONFLICT (tenant, entity_type) DO UPDATE SET
                     ioa_source = CASE WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml THEN excluded.ioa_source ELSE specs.ioa_source END,
                     csdl_xml = CASE WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml THEN excluded.csdl_xml ELSE specs.csdl_xml END,
                     content_hash = CASE WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml THEN excluded.content_hash ELSE specs.content_hash END,
                     committed = 1,
                     version = CASE WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml THEN specs.version + 1 ELSE specs.version END,
                     verified = CASE WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml THEN 0 ELSE specs.verified END,
                     verification_status = CASE WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml THEN 'pending' ELSE specs.verification_status END,
                     levels_passed = CASE WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml THEN NULL ELSE specs.levels_passed END,
                     levels_total = CASE WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml THEN NULL ELSE specs.levels_total END,
                     verification_result = CASE WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml THEN NULL ELSE specs.verification_result END,
                     updated_at = CASE WHEN specs.content_hash IS NOT excluded.content_hash OR specs.csdl_xml IS NOT excluded.csdl_xml OR specs.committed != 1 THEN datetime('now') ELSE specs.updated_at END",
                params![tenant, *entity_type, *ioa_source, *csdl_xml, *content_hash],
            )
            .await
            .map_err(storage_error)?;
        }
        for entity_type in &removed_entity_types {
            tx.execute(
                "DELETE FROM specs WHERE tenant = ?1 AND entity_type = ?2",
                params![tenant, entity_type.as_str()],
            )
            .await
            .map_err(storage_error)?;
        }
        match constraints {
            None => {}
            Some(Some(source)) => {
                tx.execute(
                    "INSERT INTO tenant_constraints \
                     (tenant, cross_invariants_toml, version, updated_at) \
                     VALUES (?1, ?2, 1, datetime('now')) \
                     ON CONFLICT (tenant) DO UPDATE SET \
                         cross_invariants_toml = excluded.cross_invariants_toml, \
                         version = tenant_constraints.version + 1, \
                         updated_at = datetime('now')",
                    params![tenant, source],
                )
                .await
                .map_err(storage_error)?;
            }
            Some(None) => {
                tx.execute(
                    "DELETE FROM tenant_constraints WHERE tenant = ?1",
                    params![tenant],
                )
                .await
                .map_err(storage_error)?;
            }
        }
        if let Some(policy) = policy {
            tx.execute(
                "INSERT INTO tenant_policies (tenant, policy_text, updated_at) \
                 VALUES (?1, ?2, datetime('now')) \
                 ON CONFLICT(tenant) DO UPDATE SET \
                     policy_text = excluded.policy_text, updated_at = datetime('now')",
                params![tenant, policy],
            )
            .await
            .map_err(storage_error)?;
        }
        if let Some(owner) = policy_owner {
            tx.execute(
                "DELETE FROM policies WHERE tenant = ?1 AND created_by = ?2",
                params![tenant, owner],
            )
            .await
            .map_err(storage_error)?;
            for (policy_id, cedar_text, created_by) in policy_entries {
                let mut digest = Sha256::new();
                digest.update(cedar_text.as_bytes());
                let policy_hash = format!("{:x}", digest.finalize());
                tx.execute(
                    "INSERT INTO policies \
                     (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
                     VALUES (?1, ?2, ?3, ?4, datetime('now'), ?5, 1) \
                     ON CONFLICT(tenant, policy_id) DO UPDATE SET \
                         cedar_text = excluded.cedar_text, policy_hash = excluded.policy_hash, \
                         created_by = excluded.created_by, created_at = datetime('now'), enabled = 1",
                    params![tenant, *policy_id, *cedar_text, policy_hash, *created_by],
                )
                .await
                .map_err(storage_error)?;
            }
        }
        for (module_name, wasm_bytes, sha256_hash, source) in wasm_modules {
            let replace_uploaded_wasm = *source == "bundled-replace-upload";
            let persisted_source = if replace_uploaded_wasm {
                "bundled"
            } else {
                *source
            };
            tx.execute(
                "INSERT INTO wasm_modules \
                 (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at, source) \
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, datetime('now'), ?6) \
                 ON CONFLICT(tenant, module_name) DO UPDATE SET \
                     wasm_bytes = excluded.wasm_bytes, sha256_hash = excluded.sha256_hash, \
                     version = wasm_modules.version + 1, size_bytes = excluded.size_bytes, \
                     updated_at = datetime('now'), source = excluded.source \
                 WHERE wasm_modules.sha256_hash IS NOT excluded.sha256_hash \
                    AND (?7 OR excluded.source = 'upload' OR wasm_modules.source = 'bundled')",
                params![
                    tenant,
                    *module_name,
                    wasm_bytes.to_vec(),
                    *sha256_hash,
                    wasm_bytes.len() as i64,
                    persisted_source,
                    replace_uploaded_wasm,
                ],
            )
            .await
            .map_err(storage_error)?;
        }
        if let Some(record) = os_app {
            tx.execute(
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
                    tenant,
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
                    record.status.as_str(),
                ],
            )
            .await
            .map_err(storage_error)?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(removed_entity_types)
    }
}
