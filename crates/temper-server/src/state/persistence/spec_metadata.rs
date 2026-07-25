use sqlx::types::Json;
use temper_store_turso::TursoSpecVerificationUpdate;

use super::super::ServerState;
use super::TenantMetadataBackend;
use crate::registry::EntityVerificationResult;

async fn stage_postgres_spec_source(
    pool: &sqlx::PgPool,
    tenant: &str,
    entity_type: &str,
    ioa_source: &str,
    csdl_xml: &str,
    content_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO staged_specs \
         (tenant, entity_type, ioa_source, csdl_xml, content_hash, version, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 1, now()) \
         ON CONFLICT (tenant, entity_type) DO UPDATE SET \
             ioa_source = EXCLUDED.ioa_source, \
             csdl_xml = EXCLUDED.csdl_xml, \
             content_hash = EXCLUDED.content_hash, \
             version = CASE \
                 WHEN staged_specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash \
                   OR staged_specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml \
                 THEN staged_specs.version + 1 \
                 ELSE staged_specs.version \
             END, \
             updated_at = CASE \
                 WHEN staged_specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash \
                   OR staged_specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml \
                 THEN now() \
                 ELSE staged_specs.updated_at \
             END",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(ioa_source)
    .bind(csdl_xml)
    .bind(content_hash)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn delete_postgres_spec_source(
    pool: &sqlx::PgPool,
    tenant: &str,
    entity_type: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH staged AS ( \
             DELETE FROM staged_specs WHERE tenant = $1 AND entity_type = $2 \
         ) SELECT tombstone_spec_declaration_authority($1, $2)",
    )
    .bind(tenant)
    .bind(entity_type)
    .execute(pool)
    .await
    .map(|_| ())
}

impl ServerState {
    /// Stage candidate catalog bytes without changing committed authority.
    #[cfg(feature = "observe")]
    pub(crate) async fn stage_spec_catalog_update(
        &self,
        tenant: &str,
        ioa_sources: &std::collections::BTreeMap<String, String>,
        csdl_xml: &str,
    ) -> Result<(), String> {
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return Ok(());
        };
        for (entity_type, ioa_source) in ioa_sources {
            let content_hash = temper_store_turso::spec_content_hash(ioa_source);
            match &backend {
                TenantMetadataBackend::Postgres(pool) => stage_postgres_spec_source(
                    pool,
                    tenant,
                    entity_type,
                    ioa_source,
                    csdl_xml,
                    &content_hash,
                )
                .await
                .map_err(|error| {
                    format!("failed to stage spec {tenant}/{entity_type} in postgres: {error}")
                })?,
                TenantMetadataBackend::Turso(store) => store
                    .upsert_spec(tenant, entity_type, ioa_source, csdl_xml, &content_hash)
                    .await
                    .map_err(|error| {
                        format!("failed to stage spec {tenant}/{entity_type} in turso: {error}")
                    })?,
                TenantMetadataBackend::Redis => {
                    return Err(Self::redis_ephemeral_error("Spec source staging"));
                }
            }
        }
        Ok(())
    }

    /// Upsert a spec source into the persistence backend (Postgres or Turso).
    pub async fn upsert_spec_source(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
    ) -> Result<(), String> {
        let content_hash = temper_store_turso::spec_content_hash(ioa_source);
        if let Some(backend) = self.tenant_metadata_backend(tenant).await {
            match backend {
                TenantMetadataBackend::Postgres(pool) => stage_postgres_spec_source(
                    &pool,
                    tenant,
                    entity_type,
                    ioa_source,
                    csdl_xml,
                    &content_hash,
                )
                .await
                .map_err(|e| {
                    format!("failed to upsert spec {tenant}/{entity_type} in postgres: {e}")
                }),
                TenantMetadataBackend::Turso(turso) => turso
                    .upsert_spec(tenant, entity_type, ioa_source, csdl_xml, &content_hash)
                    .await
                    .map_err(|e| {
                        format!("failed to upsert spec {tenant}/{entity_type} in turso: {e}")
                    }),
                TenantMetadataBackend::Redis => {
                    Err(Self::redis_ephemeral_error("Spec source persistence"))
                }
            }?;
        }
        self.persist_event_store_spec_declaration(tenant, entity_type, &content_hash)
            .await
    }

    /// Delete a persisted spec source while retaining the backend's declaration
    /// tombstone used to fence stale writers and resume vector-row purging.
    pub async fn delete_spec_source(&self, tenant: &str, entity_type: &str) -> Result<(), String> {
        if let Some(backend) = self.tenant_metadata_backend(tenant).await {
            match backend {
                TenantMetadataBackend::Postgres(pool) => {
                    delete_postgres_spec_source(&pool, tenant, entity_type)
                        .await
                        .map_err(|e| {
                            format!("failed to delete spec {tenant}/{entity_type} in postgres: {e}")
                        })
                }
                TenantMetadataBackend::Turso(turso) => {
                    turso.delete_spec(tenant, entity_type).await.map_err(|e| {
                        format!("failed to delete spec {tenant}/{entity_type} in turso: {e}")
                    })
                }
                TenantMetadataBackend::Redis => {
                    Err(Self::redis_ephemeral_error("Spec source deletion"))
                }
            }?;
        }
        self.persist_event_store_spec_declaration(tenant, entity_type, "absent:v1")
            .await
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
            TenantMetadataBackend::Postgres(pool) => {
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
                    .execute(&pool)
                    .await
                    .map_err(|e| format!("failed to upsert tenant constraints for {tenant}: {e}"))?;
                } else {
                    sqlx::query("DELETE FROM tenant_constraints WHERE tenant = $1")
                        .bind(tenant)
                        .execute(&pool)
                        .await
                        .map_err(|e| {
                            format!("failed to clear tenant constraints for {tenant}: {e}")
                        })?;
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
            TenantMetadataBackend::Postgres(pool) => {
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
                .execute(&pool)
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

#[cfg(test)]
mod tests {
    use temper_store_postgres::{
        PostgresEventStore, PostgresSpecVerificationUpdate, migration::run_migrations,
    };

    use super::{delete_postgres_spec_source, stage_postgres_spec_source};

    #[test]
    fn postgres_hot_update_and_delete_fence_stale_verification() {
        let Ok(database_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        sqlx::__rt::test_block_on(async {
            let pool = sqlx::PgPool::connect(&database_url).await.expect("connect");
            run_migrations(&pool).await.expect("migrate");
            let store = PostgresEventStore::new(pool.clone());
            let tenant = format!("tenant-server-spec-race-{}", uuid::Uuid::new_v4());
            let ioa_a = "[automaton]\nname = \"Item\"\n# a\n";
            let ioa_b = "[automaton]\nname = \"Item\"\n# b\n";
            let csdl = "<Schema Namespace=\"Temper.Tests\" />";
            let hash_a = temper_store_turso::spec_content_hash(ioa_a);
            let hash_b = temper_store_turso::spec_content_hash(ioa_b);
            let verified = || PostgresSpecVerificationUpdate {
                status: "completed",
                verified: true,
                levels_passed: None,
                levels_total: None,
                verification_result_json: None,
            };

            stage_postgres_spec_source(&pool, &tenant, "Item", ioa_a, csdl, &hash_a)
                .await
                .expect("stage A through server path");
            store
                .commit_verified_spec(&tenant, "Item", &hash_a, csdl, verified())
                .await
                .expect("commit A");
            stage_postgres_spec_source(&pool, &tenant, "Item", ioa_b, csdl, &hash_b)
                .await
                .expect("stage B through server path");
            store
                .commit_verified_spec(&tenant, "Item", &hash_a, csdl, verified())
                .await
                .expect_err("stale A verification must not publish B");

            let committed: (String,) = sqlx::query_as(
                "SELECT content_hash FROM specs WHERE tenant = $1 AND entity_type = 'Item'",
            )
            .bind(&tenant)
            .fetch_one(&pool)
            .await
            .expect("read committed A");
            assert_eq!(committed.0, hash_a);

            delete_postgres_spec_source(&pool, &tenant, "Item")
                .await
                .expect("delete through server path");
            store
                .commit_verified_spec(&tenant, "Item", &hash_b, csdl, verified())
                .await
                .expect_err("stale B verification must not resurrect deletion");
            let remaining: (i64,) = sqlx::query_as(
                "SELECT \
                    (SELECT COUNT(*) FROM specs WHERE tenant = $1 AND entity_type = 'Item') + \
                    (SELECT COUNT(*) FROM staged_specs WHERE tenant = $1 AND entity_type = 'Item')",
            )
            .bind(&tenant)
            .fetch_one(&pool)
            .await
            .expect("count remaining catalog rows");
            assert_eq!(remaining.0, 0);
        });
    }
}
