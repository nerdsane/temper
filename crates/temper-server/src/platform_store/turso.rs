//! Turso implementation of the backend-neutral platform store.

use super::*;
use temper_store_turso::{
    TursoEventStore, TursoInstalledAppRow, TursoRegistrySourceSnapshot, TursoSpecVerificationUpdate,
};

fn source_snapshot(source: &RegistrySourceSnapshot) -> TursoRegistrySourceSnapshot {
    TursoRegistrySourceSnapshot {
        spec_versions: source.spec_versions.clone(),
        constraint_versions: source.constraint_versions.clone(),
    }
}

fn quarantine_record(
    row: temper_store_turso::TursoRegistryQuarantineRow,
) -> RegistryQuarantineRecord {
    RegistryQuarantineRecord {
        tenant: row.tenant,
        entity_type: row.entity_type,
        spec_version: row.spec_version,
        constraint_version: row.constraint_version,
        reason: row.reason,
        source_kind: row.source_kind,
        source_line: row.source_line,
        source_column: row.source_column,
        detail: row.detail,
        acknowledged_at: row.acknowledged_at,
        created_at: row.created_at,
        last_observed_at: row.last_observed_at,
    }
}

#[async_trait::async_trait]
impl PlatformStore for TursoEventStore {
    async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), String> {
        self.upsert_spec(tenant, entity_type, ioa_source, csdl_xml, content_hash)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_specs(&self) -> Result<Vec<SpecRow>, String> {
        let rows = self.load_specs().await.map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| SpecRow {
                tenant: r.tenant,
                entity_type: r.entity_type,
                ioa_source: r.ioa_source,
                csdl_xml: r.csdl_xml,
                content_hash: r.content_hash.unwrap_or_default(),
                version: r.version,
                committed: r.committed,
                verification_status: r.verification_status,
                verified: r.verified,
                levels_passed: r.levels_passed,
                levels_total: r.levels_total,
                verification_result: r.verification_result,
                updated_at: r.updated_at,
            })
            .collect())
    }

    async fn load_tenant_constraints(&self) -> Result<Vec<TenantConstraintRow>, String> {
        self.load_tenant_constraints()
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|row| TenantConstraintRow {
                        tenant: row.tenant,
                        source: row.cross_invariants_toml,
                        version: i64::from(row.version),
                    })
                    .collect()
            })
            .map_err(|error| error.to_string())
    }

    async fn delete_spec(&self, tenant: &str, entity_type: &str) -> Result<(), String> {
        self.delete_spec(tenant, entity_type)
            .await
            .map_err(|e| e.to_string())
    }

    async fn commit_specs(&self, tenant: &str) -> Result<(), String> {
        self.commit_specs(tenant).await.map_err(|e| e.to_string())
    }
    async fn delete_uncommitted_specs(&self) -> Result<usize, String> {
        self.delete_uncommitted_specs()
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_verification_cache(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, (String, bool)>, String> {
        self.load_verification_cache(tenant)
            .await
            .map_err(|e| e.to_string())
    }

    async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        update: SpecVerificationUpdate<'_>,
    ) -> Result<(), String> {
        let turso_update = TursoSpecVerificationUpdate {
            status: update.status,
            verified: update.verified,
            levels_passed: update.levels_passed,
            levels_total: update.levels_total,
            verification_result_json: update.verification_result_json,
        };
        self.persist_spec_verification(tenant, entity_type, turso_update)
            .await
            .map_err(|e| e.to_string())
    }

    async fn replace_registry_restore_quarantines(
        &self,
        source: &RegistrySourceSnapshot,
        active: &[RegistryQuarantineUpsert<'_>],
    ) -> Result<bool, String> {
        validate_registry_quarantine_snapshot(active)?;
        let rows = active
            .iter()
            .map(|row| temper_store_turso::TursoRegistryQuarantineUpsert {
                tenant: row.tenant,
                entity_type: row.entity_type,
                spec_version: row.spec_version,
                constraint_version: row.constraint_version,
                reason: row.reason,
                source_kind: row.source_kind,
                source_line: row.source_line,
                source_column: row.source_column,
                detail: row.detail,
            })
            .collect::<Vec<_>>();
        self.replace_registry_restore_quarantines(&source_snapshot(source), &rows)
            .await
            .map_err(|error| error.to_string())
    }

    async fn replace_registry_restore_quarantines_for_tenant(
        &self,
        tenant: &str,
        source: &RegistrySourceSnapshot,
        active: &[RegistryQuarantineUpsert<'_>],
    ) -> Result<bool, String> {
        validate_registry_quarantine_snapshot(active)?;
        let rows = active
            .iter()
            .map(|row| temper_store_turso::TursoRegistryQuarantineUpsert {
                tenant: row.tenant,
                entity_type: row.entity_type,
                spec_version: row.spec_version,
                constraint_version: row.constraint_version,
                reason: row.reason,
                source_kind: row.source_kind,
                source_line: row.source_line,
                source_column: row.source_column,
                detail: row.detail,
            })
            .collect::<Vec<_>>();
        self.replace_registry_restore_quarantines_for_tenant(
            tenant,
            &source_snapshot(source),
            &rows,
        )
        .await
        .map_err(|error| error.to_string())
    }

    async fn resolve_registry_restore_quarantines(
        &self,
        source: &RegistrySourceSnapshot,
        resolutions: &[RegistryQuarantineResolution<'_>],
    ) -> Result<bool, String> {
        let rows = resolutions
            .iter()
            .map(
                |row| temper_store_turso::TursoRegistryQuarantineResolution {
                    tenant: row.tenant,
                    entity_type: row.entity_type,
                    quarantined_version: row.quarantined_version,
                    quarantined_constraint_version: row.quarantined_constraint_version,
                },
            )
            .collect::<Vec<_>>();
        self.resolve_registry_restore_quarantines(&source_snapshot(source), &rows)
            .await
            .map_err(|error| error.to_string())
    }

    async fn acknowledge_registry_restore_quarantine(
        &self,
        tenant: &str,
        entity_type: &str,
        spec_version: i64,
        constraint_version: Option<i64>,
    ) -> Result<Option<(i64, Option<i64>)>, String> {
        self.acknowledge_registry_restore_quarantine(
            tenant,
            entity_type,
            spec_version,
            constraint_version,
        )
        .await
        .map_err(|error| error.to_string())
    }

    async fn load_registry_restore_quarantines(
        &self,
    ) -> Result<Vec<RegistryQuarantineRecord>, String> {
        self.load_registry_restore_quarantines()
            .await
            .map(|rows| rows.into_iter().map(quarantine_record).collect())
            .map_err(|error| error.to_string())
    }

    async fn load_registry_restore_quarantines_for_tenant(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<RegistryQuarantineRecord>, String> {
        self.load_registry_restore_quarantines_for_tenant(tenant, limit)
            .await
            .map(|rows| rows.into_iter().map(quarantine_record).collect())
            .map_err(|error| error.to_string())
    }

    async fn upsert_tenant_policy(&self, tenant: &str, policy_text: &str) -> Result<(), String> {
        self.upsert_tenant_policy(tenant, policy_text)
            .await
            .map_err(|e| e.to_string())
    }

    async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: &str,
    ) -> Result<(), String> {
        self.upsert_tenant_constraints(tenant, cross_invariants_toml)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, String> {
        self.load_tenant_policies().await.map_err(|e| e.to_string())
    }

    async fn load_policy_entries(&self) -> Result<Vec<PolicyEntryRow>, String> {
        let rows = self.load_all_policies().await.map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|row| PolicyEntryRow {
                tenant: row.tenant,
                policy_id: row.policy_id,
                cedar_text: row.cedar_text,
                enabled: row.enabled,
            })
            .collect())
    }

    async fn is_app_installed(&self, tenant: &str, app_name: &str) -> Result<bool, String> {
        self.is_app_installed(tenant, app_name)
            .await
            .map_err(|e| e.to_string())
    }

    async fn record_installed_app(&self, tenant: &str, app_name: &str) -> Result<(), String> {
        self.record_installed_app(tenant, app_name)
            .await
            .map_err(|e| e.to_string())
    }

    async fn record_installed_app_metadata(
        &self,
        record: &InstalledAppRecord,
    ) -> Result<(), String> {
        let row = TursoInstalledAppRow {
            tenant_id: record.tenant.clone(),
            app_name: record.app_name.clone(),
            source_kind: record.source_kind.clone(),
            app_ref: record.app_ref.clone(),
            version_hash: record.version_hash.clone(),
            pinned_version_hash: record.pinned_version_hash.clone(),
            current_version_hash: record.current_version_hash.clone(),
            follow_policy: record.follow_policy.clone(),
            closure_id: record.closure_id.clone(),
            registry_url: record.registry_url.clone(),
            registry_tenant: record.registry_tenant.clone(),
            app_version: record.app_version.clone(),
            bundle_digest: record.bundle_digest.clone(),
            spec_digest: record.spec_digest.clone(),
            policy_digest: record.policy_digest.clone(),
            wasm_digest: record.wasm_digest.clone(),
            content_digest: record.content_digest.clone(),
            seed_digest: record.seed_digest.clone(),
            installed_at: record.installed_at.clone().unwrap_or_default(),
            last_reconciled_at: record.last_reconciled_at.clone(),
            status: record.status.clone(),
        };
        self.record_installed_app_metadata(&row)
            .await
            .map_err(|e| e.to_string())
    }

    async fn get_installed_app(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<Option<InstalledAppRecord>, String> {
        self.get_installed_app(tenant, app_name)
            .await
            .map(|row| {
                row.map(|row| InstalledAppRecord {
                    tenant: row.tenant_id,
                    app_name: row.app_name,
                    source_kind: row.source_kind,
                    app_ref: row.app_ref,
                    version_hash: row.version_hash,
                    pinned_version_hash: row.pinned_version_hash,
                    current_version_hash: row.current_version_hash,
                    follow_policy: row.follow_policy,
                    closure_id: row.closure_id,
                    registry_url: row.registry_url,
                    registry_tenant: row.registry_tenant,
                    app_version: row.app_version,
                    bundle_digest: row.bundle_digest,
                    spec_digest: row.spec_digest,
                    policy_digest: row.policy_digest,
                    wasm_digest: row.wasm_digest,
                    content_digest: row.content_digest,
                    seed_digest: row.seed_digest,
                    installed_at: Some(row.installed_at),
                    last_reconciled_at: row.last_reconciled_at,
                    status: row.status,
                })
            })
            .map_err(|e| e.to_string())
    }

    async fn list_all_installed_apps(&self) -> Result<Vec<(String, String)>, String> {
        self.list_all_installed_apps()
            .await
            .map_err(|e| e.to_string())
    }

    async fn upsert_pending_decision(
        &self,
        id: &str,
        tenant: &str,
        status: &str,
        data: &str,
    ) -> Result<(), String> {
        self.upsert_pending_decision(id, tenant, status, data)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_pending_decisions(&self, limit: usize) -> Result<Vec<String>, String> {
        self.load_pending_decisions(limit as i64)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_all_wasm_modules(&self, tenant: &str) -> Result<Vec<WasmModuleRow>, String> {
        let rows = self
            .load_all_wasm_modules(tenant)
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| WasmModuleRow {
                tenant: r.tenant,
                module_name: r.module_name,
                wasm_bytes: r.wasm_bytes,
                sha256_hash: r.sha256_hash,
                source: r.source,
            })
            .collect())
    }

    async fn load_wasm_modules_all_tenants(&self) -> Result<Vec<WasmModuleRow>, String> {
        let rows = self
            .load_wasm_modules_all_tenants()
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| WasmModuleRow {
                tenant: r.tenant,
                module_name: r.module_name,
                wasm_bytes: r.wasm_bytes,
                sha256_hash: r.sha256_hash,
                source: r.source,
            })
            .collect())
    }

    async fn upsert_wasm_module(
        &self,
        tenant: &str,
        name: &str,
        bytes: &[u8],
        hash: &str,
        source: &str,
    ) -> Result<(), String> {
        self.upsert_wasm_module(tenant, name, bytes, hash, source)
            .await
            .map_err(|e| e.to_string())
    }
}
