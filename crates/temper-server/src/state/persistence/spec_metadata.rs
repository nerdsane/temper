use sqlx::types::Json;
use temper_store_turso::TursoSpecVerificationUpdate;

use super::super::ServerState;
use super::MetadataBackend;
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
        let Some(backend) = self.metadata_backend() else {
            return Ok(());
        };

        match backend {
            MetadataBackend::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO specs \
                     (tenant, entity_type, ioa_source, csdl_xml, version, verified, verification_status, updated_at) \
                     VALUES ($1, $2, $3, $4, 1, false, 'pending', now()) \
                     ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                         ioa_source = EXCLUDED.ioa_source, \
                         csdl_xml = EXCLUDED.csdl_xml, \
                         version = specs.version + 1, \
                         verified = false, \
                         verification_status = 'pending', \
                         levels_passed = NULL, \
                         levels_total = NULL, \
                         verification_result = NULL, \
                         updated_at = now()",
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(ioa_source)
                .bind(csdl_xml)
                .execute(pool)
                .await
                .map(|_| ())
                .map_err(|e| format!("failed to upsert spec {tenant}/{entity_type} in postgres: {e}"))
            }
            MetadataBackend::Turso(turso) => turso
                .upsert_spec(tenant, entity_type, ioa_source, csdl_xml)
                .await
                .map_err(|e| format!("failed to upsert spec {tenant}/{entity_type} in turso: {e}")),
            MetadataBackend::Redis => Err(Self::redis_ephemeral_error("Spec source persistence")),
        }
    }

    /// Upsert tenant-level cross-invariant definitions.
    pub async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: Option<&str>,
    ) -> Result<(), String> {
        let Some(backend) = self.metadata_backend() else {
            return Ok(());
        };

        match backend {
            MetadataBackend::Postgres(pool) => {
                if let Some(source) = cross_invariants_toml {
                    sqlx::query(
                        "INSERT INTO tenant_constraints (tenant, cross_invariants_toml, version, updated_at) \
                         VALUES ($1, $2, 1, now()) \
                         ON CONFLICT (tenant) DO UPDATE SET \
                            cross_invariants_toml = EXCLUDED.cross_invariants_toml, \
                            version = tenant_constraints.version + 1, \
                            updated_at = now()",
                    )
                    .bind(tenant)
                    .bind(source)
                    .execute(pool)
                    .await
                    .map_err(|e| format!("failed to upsert tenant constraints for {tenant}: {e}"))?;
                } else {
                    sqlx::query("DELETE FROM tenant_constraints WHERE tenant = $1")
                        .bind(tenant)
                        .execute(pool)
                        .await
                        .map_err(|e| {
                            format!("failed to clear tenant constraints for {tenant}: {e}")
                        })?;
                }
                Ok(())
            }
            MetadataBackend::Turso(turso) => {
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
            MetadataBackend::Redis => {
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
        let Some(backend) = self.metadata_backend() else {
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
            MetadataBackend::Postgres(pool) => {
                sqlx::query(
                    "UPDATE specs SET \
                         verified = $3, \
                         verification_status = $4, \
                         levels_passed = $5, \
                         levels_total = $6, \
                         verification_result = $7, \
                         updated_at = now() \
                     WHERE tenant = $1 AND entity_type = $2",
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(verified)
                .bind(status)
                .bind(levels_passed)
                .bind(levels_total)
                .bind(verification_result.map(Json))
                .execute(pool)
                .await
                .map(|_| ())
                .map_err(|e| {
                    format!(
                        "failed to persist spec verification status for {tenant}/{entity_type} ({status}): {e}"
                    )
                })
            }
            MetadataBackend::Turso(turso) => {
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
            MetadataBackend::Redis => Err(Self::redis_ephemeral_error("Spec verification persistence")),
        }
    }
}
