//! Storage backend connection and persistence functions (Postgres, Turso).

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use temper_evolution::PostgresRecordStore;
use temper_runtime::tenant::TenantId;
use temper_server::event_store::ServerEventStore;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_spec::csdl::parse_csdl;
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::TursoEventStore;

use super::LoadedTenantSpecs;

#[derive(sqlx::FromRow)]
pub(super) struct PersistedSpecRow {
    pub tenant: String,
    pub entity_type: String,
    pub ioa_source: String,
    pub csdl_xml: Option<String>,
    pub verification_status: String,
    pub verified: bool,
    pub levels_passed: Option<i32>,
    pub levels_total: Option<i32>,
    pub verification_result: Option<serde_json::Value>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(sqlx::FromRow)]
struct PersistedTenantConstraintRow {
    tenant: String,
    cross_invariants_toml: String,
}

pub(super) async fn connect_postgres_store(database_url: &str) -> Result<(ServerEventStore, sqlx::PgPool)> {
    println!("  Connecting to Postgres...");
    let pool = sqlx::PgPool::connect(database_url)
        .await
        .context("Failed to connect to Postgres")?;
    temper_store_postgres::migration::run_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to run migrations: {e}"))?;
    let pg_record_store: PostgresRecordStore = PostgresRecordStore::new(pool.clone());
    pg_record_store
        .migrate()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to migrate evolution_records: {e}"))?;
    println!("  Postgres connected, migrations applied.");
    Ok((
        ServerEventStore::Postgres(PostgresEventStore::new(pool.clone())),
        pool,
    ))
}

pub(super) fn redact_connection_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some(at_idx) = rest.find('@') else {
        return url.to_string();
    };
    let creds = &rest[..at_idx];
    let host_and_path = &rest[at_idx + 1..];
    if let Some((user, _password)) = creds.split_once(':') {
        format!("{scheme}://{user}:***@{host_and_path}")
    } else {
        format!("{scheme}://***@{host_and_path}")
    }
}

pub(super) async fn upsert_loaded_specs_to_postgres(
    pool: &sqlx::PgPool,
    tenant: &str,
    loaded: &LoadedTenantSpecs,
) -> Result<()> {
    for (entity_type, ioa_source) in &loaded.ioa_sources {
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
        .bind(&loaded.csdl_xml)
        .execute(pool)
        .await
        .with_context(|| format!("Failed to persist spec {tenant}/{entity_type}"))?;
    }
    if let Some(source) = loaded.cross_invariants_toml.as_deref() {
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
        .with_context(|| format!("Failed to persist tenant constraints for {tenant}"))?;
    } else {
        sqlx::query("DELETE FROM tenant_constraints WHERE tenant = $1")
            .bind(tenant)
            .execute(pool)
            .await
            .with_context(|| format!("Failed to clear tenant constraints for {tenant}"))?;
    }
    Ok(())
}

fn persisted_status_to_registry_status(row: &PersistedSpecRow) -> VerificationStatus {
    let status = row.verification_status.to_lowercase();
    match status.as_str() {
        "pending" => VerificationStatus::Pending,
        "running" => VerificationStatus::Running,
        _ => {
            if let Some(value) = row.verification_result.clone() {
                if let Ok(result) = serde_json::from_value::<EntityVerificationResult>(value) {
                    return VerificationStatus::Completed(result);
                }
            }

            let all_passed = status == "passed" || row.verified;
            let levels_passed = row
                .levels_passed
                .unwrap_or(if all_passed { 1 } else { 0 })
                .max(0) as usize;
            let levels_total = row.levels_total.unwrap_or(levels_passed as i32).max(0) as usize;
            let levels = if levels_total > 0 {
                (0..levels_total)
                    .map(|idx| EntityLevelSummary {
                        level: format!("L{idx}"),
                        passed: idx < levels_passed,
                        summary: if idx < levels_passed {
                            "Restored from persisted verification summary".to_string()
                        } else {
                            "Restored failed verification level".to_string()
                        },
                        details: None,
                    })
                    .collect()
            } else {
                vec![EntityLevelSummary {
                    level: "Persisted".to_string(),
                    passed: all_passed,
                    summary: format!("Restored status '{}'", row.verification_status),
                    details: None,
                }]
            };
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed,
                levels,
                verified_at: row.updated_at.to_rfc3339(),
            })
        }
    }
}

pub(super) async fn load_registry_from_postgres(
    registry: &mut SpecRegistry,
    pool: &sqlx::PgPool,
) -> Result<usize> {
    let rows: Vec<PersistedSpecRow> = sqlx::query_as(
        "SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, \
                levels_passed, levels_total, verification_result, updated_at \
         FROM specs \
         ORDER BY tenant, entity_type",
    )
    .fetch_all(pool)
    .await
    .context("Failed to read specs from Postgres")?;

    let constraints_rows: Vec<PersistedTenantConstraintRow> = sqlx::query_as(
        "SELECT tenant, cross_invariants_toml \
         FROM tenant_constraints \
         ORDER BY tenant",
    )
    .fetch_all(pool)
    .await
    .context("Failed to read tenant constraints from Postgres")?;

    let mut constraints_by_tenant: BTreeMap<String, String> = constraints_rows
        .into_iter()
        .map(|row| (row.tenant, row.cross_invariants_toml))
        .collect();

    if rows.is_empty() {
        return Ok(0);
    }

    let mut grouped: BTreeMap<String, Vec<PersistedSpecRow>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.tenant.clone()).or_default().push(row);
    }

    let mut restored_specs = 0usize;
    for (tenant, tenant_rows) in grouped {
        let csdl_xml = tenant_rows
            .iter()
            .find_map(|row| row.csdl_xml.clone())
            .unwrap_or_default();
        if csdl_xml.trim().is_empty() {
            eprintln!("Warning: skipping restored tenant '{tenant}' due to missing CSDL");
            continue;
        }
        let csdl = parse_csdl(&csdl_xml)
            .with_context(|| format!("Failed to parse restored CSDL for tenant '{tenant}'"))?;

        let ioa_owned: Vec<(String, String)> = tenant_rows
            .iter()
            .map(|row| (row.entity_type.clone(), row.ioa_source.clone()))
            .collect();
        let ioa_pairs: Vec<(&str, &str)> = ioa_owned
            .iter()
            .map(|(entity_type, ioa)| (entity_type.as_str(), ioa.as_str()))
            .collect();

        let cross_invariants_toml = constraints_by_tenant.remove(&tenant);
        registry.register_tenant_with_reactions_and_constraints(
            tenant.as_str(),
            csdl,
            csdl_xml,
            &ioa_pairs,
            Vec::new(),
            cross_invariants_toml,
        );
        let tenant_id = TenantId::new(&tenant);
        for row in &tenant_rows {
            registry.set_verification_status(
                &tenant_id,
                &row.entity_type,
                persisted_status_to_registry_status(row),
            );
            restored_specs += 1;
        }
    }

    Ok(restored_specs)
}

/// Load specs from Turso into a registry (mirrors `load_registry_from_postgres`).
pub(super) async fn load_registry_from_turso(
    registry: &mut SpecRegistry,
    turso: &TursoEventStore,
) -> Result<usize> {
    let rows = turso
        .load_specs()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read specs from Turso: {e}"))?;
    let constraints_rows = turso
        .load_tenant_constraints()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read tenant constraints from Turso: {e}"))?;

    let mut constraints_by_tenant: BTreeMap<String, String> = constraints_rows
        .into_iter()
        .map(|row| (row.tenant, row.cross_invariants_toml))
        .collect();

    if rows.is_empty() {
        return Ok(0);
    }

    let mut grouped: BTreeMap<String, Vec<temper_store_turso::TursoSpecRow>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.tenant.clone()).or_default().push(row);
    }

    let mut restored_specs = 0usize;
    for (tenant, tenant_rows) in grouped {
        let csdl_xml = tenant_rows
            .iter()
            .find_map(|row| row.csdl_xml.clone())
            .unwrap_or_default();
        if csdl_xml.trim().is_empty() {
            eprintln!("Warning: skipping restored tenant '{tenant}' due to missing CSDL");
            continue;
        }
        let csdl = parse_csdl(&csdl_xml)
            .with_context(|| format!("Failed to parse restored CSDL for tenant '{tenant}'"))?;

        let ioa_owned: Vec<(String, String)> = tenant_rows
            .iter()
            .map(|row| (row.entity_type.clone(), row.ioa_source.clone()))
            .collect();
        let ioa_pairs: Vec<(&str, &str)> = ioa_owned
            .iter()
            .map(|(entity_type, ioa)| (entity_type.as_str(), ioa.as_str()))
            .collect();

        let cross_invariants_toml = constraints_by_tenant.remove(&tenant);
        registry.register_tenant_with_reactions_and_constraints(
            tenant.as_str(),
            csdl,
            csdl_xml,
            &ioa_pairs,
            Vec::new(),
            cross_invariants_toml,
        );
        let tenant_id = TenantId::new(&tenant);
        for row in &tenant_rows {
            registry.set_verification_status(
                &tenant_id,
                &row.entity_type,
                turso_status_to_registry_status(row),
            );
            restored_specs += 1;
        }
    }

    Ok(restored_specs)
}

/// Convert a Turso spec row's verification status to a registry VerificationStatus.
fn turso_status_to_registry_status(row: &temper_store_turso::TursoSpecRow) -> VerificationStatus {
    let status = row.verification_status.to_lowercase();
    match status.as_str() {
        "pending" => VerificationStatus::Pending,
        "running" => VerificationStatus::Running,
        _ => {
            if let Some(ref json_str) = row.verification_result {
                if let Ok(result) = serde_json::from_str::<EntityVerificationResult>(json_str) {
                    return VerificationStatus::Completed(result);
                }
            }

            let all_passed = status == "passed" || row.verified;
            let levels_passed = row
                .levels_passed
                .unwrap_or(if all_passed { 1 } else { 0 })
                .max(0) as usize;
            let levels_total = row.levels_total.unwrap_or(levels_passed as i32).max(0) as usize;
            let levels = if levels_total > 0 {
                (0..levels_total)
                    .map(|idx| EntityLevelSummary {
                        level: format!("L{idx}"),
                        passed: idx < levels_passed,
                        summary: if idx < levels_passed {
                            "Restored from Turso verification summary".to_string()
                        } else {
                            "Restored failed verification level".to_string()
                        },
                        details: None,
                    })
                    .collect()
            } else {
                vec![EntityLevelSummary {
                    level: "Persisted".to_string(),
                    passed: all_passed,
                    summary: format!("Restored status '{}'", row.verification_status),
                    details: None,
                }]
            };
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed,
                levels,
                verified_at: row.updated_at.clone(),
            })
        }
    }
}

/// Upsert loaded specs to Turso (mirrors `upsert_loaded_specs_to_postgres`).
pub(super) async fn upsert_loaded_specs_to_turso(
    turso: &TursoEventStore,
    tenant: &str,
    loaded: &LoadedTenantSpecs,
) -> Result<()> {
    for (entity_type, ioa_source) in &loaded.ioa_sources {
        turso
            .upsert_spec(tenant, entity_type, ioa_source, &loaded.csdl_xml)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to persist spec {tenant}/{entity_type} in Turso: {e}")
            })?;
    }
    if let Some(source) = loaded.cross_invariants_toml.as_deref() {
        turso
            .upsert_tenant_constraints(tenant, source)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to persist tenant constraints for {tenant} in Turso: {e}")
            })?;
    } else {
        turso.delete_tenant_constraints(tenant).await.map_err(|e| {
            anyhow::anyhow!("Failed to clear tenant constraints for {tenant} in Turso: {e}")
        })?;
    }
    Ok(())
}
