use std::collections::BTreeMap;

use sqlx::types::Json;
use temper_store_turso::TursoSpecVerificationUpdate;

use super::super::ServerState;
use super::TenantMetadataBackend;
use crate::registry::EntityVerificationResult;

impl ServerState {
    /// Upsert a spec source into the persistence backend (Postgres or Turso).
    pub async fn upsert_spec_source(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
    ) -> Result<(), String> {
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return Ok(());
        };

        match backend {
            TenantMetadataBackend::Postgres(store) => {
                let hash = temper_store_turso::spec_content_hash(ioa_source);
                store
                    .upsert_spec(tenant, entity_type, ioa_source, csdl_xml, &hash)
                    .await
                    .map_err(|e| {
                        format!("failed to upsert spec {tenant}/{entity_type} in postgres: {e}")
                    })
            }
            TenantMetadataBackend::Turso(turso) => {
                let hash = temper_store_turso::spec_content_hash(ioa_source);
                turso
                    .upsert_spec(tenant, entity_type, ioa_source, csdl_xml, &hash)
                    .await
                    .map_err(|e| {
                        format!("failed to upsert spec {tenant}/{entity_type} in turso: {e}")
                    })
            }
            TenantMetadataBackend::Redis => {
                Err(Self::redis_ephemeral_error("Spec source persistence"))
            }
        }
    }

    /// Commit an install whose specs all passed verification: exactly these
    /// sources with this CSDL, plus the removal of `removed` types (a
    /// replace-mode load dropped them), in one transaction. Returns whether
    /// the install is now committed (false: another install replaced a row
    /// first), or `None` when no durable backend is configured.
    pub async fn commit_spec_sources(
        &self,
        tenant: &str,
        ioa_sources: &BTreeMap<String, String>,
        csdl_xml: &str,
        removed: &[String],
    ) -> Result<Option<bool>, String> {
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return Ok(None);
        };
        let hashes: Vec<(&str, String)> = ioa_sources
            .iter()
            .map(|(entity_type, ioa_source)| {
                (
                    entity_type.as_str(),
                    temper_store_turso::spec_content_hash(ioa_source),
                )
            })
            .collect();
        let versions: Vec<(&str, &str, &str)> = hashes
            .iter()
            .map(|(entity_type, hash)| (*entity_type, hash.as_str(), csdl_xml))
            .collect();
        let removed: Vec<&str> = removed.iter().map(String::as_str).collect();

        match backend {
            TenantMetadataBackend::Postgres(store) => store
                .commit_spec_versions(tenant, &versions, &removed)
                .await
                .map(Some)
                .map_err(|e| format!("failed to commit specs for {tenant} in postgres: {e}")),
            TenantMetadataBackend::Turso(turso) => turso
                .commit_spec_versions(tenant, &versions, &removed)
                .await
                .map(Some)
                .map_err(|e| format!("failed to commit specs for {tenant} in turso: {e}")),
            TenantMetadataBackend::Redis => Err(Self::redis_ephemeral_error("Spec commit")),
        }
    }

    /// Upsert tenant-level cross-invariant definitions.
    pub async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: Option<&str>,
    ) -> Result<(), String> {
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return Ok(());
        };

        match backend {
            TenantMetadataBackend::Postgres(store) => {
                if let Some(source) = cross_invariants_toml {
                    sqlx::query(
                        &store.schema().qualify_sql("INSERT INTO {schema}tenant_constraints (tenant, cross_invariants_toml, version, updated_at) \
                         VALUES ($1, $2, 1, now()) \
                         ON CONFLICT (tenant) DO UPDATE SET \
                            cross_invariants_toml = EXCLUDED.cross_invariants_toml, \
                            version = tenant_constraints.version + 1, \
                            updated_at = now()"),
                    )
                    .bind(tenant)
                    .bind(source)
                    .execute(store.pool())
                    .await
                    .map_err(|e| format!("failed to upsert tenant constraints for {tenant}: {e}"))?;
                } else {
                    sqlx::query(
                        &store.schema().qualify_sql(
                            "DELETE FROM {schema}tenant_constraints WHERE tenant = $1",
                        ),
                    )
                    .bind(tenant)
                    .execute(store.pool())
                    .await
                    .map_err(|e| format!("failed to clear tenant constraints for {tenant}: {e}"))?;
                }
                Ok(())
            }
            TenantMetadataBackend::Turso(turso) => {
                if let Some(source) = cross_invariants_toml {
                    turso
                        .upsert_tenant_constraints(tenant, source)
                        .await
                        .map_err(|e| {
                            format!(
                                "failed to upsert tenant constraints for {tenant} in turso: {e}"
                            )
                        })?;
                } else {
                    turso.delete_tenant_constraints(tenant).await.map_err(|e| {
                        format!("failed to clear tenant constraints for {tenant} in turso: {e}")
                    })?;
                }
                Ok(())
            }
            TenantMetadataBackend::Redis => {
                Err(Self::redis_ephemeral_error("Tenant constraint persistence"))
            }
        }
    }

    /// Persist verification summary for a spec (Postgres, Turso, or skip for Redis).
    pub async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        status: &str,
        result: Option<&EntityVerificationResult>,
    ) -> Result<(), String> {
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return Ok(());
        };

        let (verified, levels_passed, levels_total, verification_result) = match result {
            Some(r) => {
                let passed = r.levels.iter().filter(|l| l.passed).count() as i32;
                let total = r.levels.len() as i32;
                let as_json = serde_json::to_value(r).ok();
                (r.all_passed, Some(passed), Some(total), as_json)
            }
            None => (false, None, None, None),
        };

        match backend {
            TenantMetadataBackend::Postgres(store) => {
                sqlx::query(
                    &store.schema().qualify_sql("UPDATE {schema}specs SET \
                         verified = $3, \
                         verification_status = $4, \
                         levels_passed = $5, \
                         levels_total = $6, \
                         verification_result = $7, \
                         updated_at = now() \
                     WHERE tenant = $1 AND entity_type = $2"),
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(verified)
                .bind(status)
                .bind(levels_passed)
                .bind(levels_total)
                .bind(verification_result.map(Json))
                .execute(store.pool())
                .await
                .map(|_| ())
                .map_err(|e| {
                    format!(
                        "failed to persist spec verification status for {tenant}/{entity_type} ({status}): {e}"
                    )
                })
            }
            TenantMetadataBackend::Turso(turso) => {
                let result_json = verification_result
                    .as_ref()
                    .and_then(|v| serde_json::to_string(v).ok());
                turso
                    .persist_spec_verification(
                        tenant,
                        entity_type,
                        TursoSpecVerificationUpdate {
                            status,
                            verified,
                            levels_passed,
                            levels_total,
                            verification_result_json: result_json.as_deref(),
                        },
                    )
                    .await
                    .map_err(|e| {
                        format!(
                            "failed to persist spec verification status for {tenant}/{entity_type} ({status}) in turso: {e}"
                        )
                    })
            }
            TenantMetadataBackend::Redis => Err(Self::redis_ephemeral_error("Spec verification persistence")),
        }
    }
}
