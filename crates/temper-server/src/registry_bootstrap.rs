//! Persistence bootstrap — restoring a [`SpecRegistry`] from storage backends.
//!
//! Centralizes the logic for reading persisted specs from Postgres or Turso and
//! populating a `SpecRegistry` with tenant registrations and verification status.
//! This keeps storage-specific row translation out of the CLI layer.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{CsdlDocument, emit_csdl_xml, merge_csdl, parse_csdl};
use temper_store_turso::TursoEventStore;

use crate::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};

/// Common accessors for spec rows from different storage backends.
trait SpecRowLike {
    fn verification_status(&self) -> &str;
    fn verified(&self) -> bool;
    fn levels_passed(&self) -> Option<i32>;
    fn levels_total(&self) -> Option<i32>;
    fn updated_at_rfc3339(&self) -> String;
    fn try_parse_verification_result(&self) -> Option<EntityVerificationResult>;
}

/// Postgres-backed spec row.
#[derive(sqlx::FromRow)]
pub struct PersistedSpecRow {
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

impl SpecRowLike for PersistedSpecRow {
    fn verification_status(&self) -> &str {
        &self.verification_status
    }
    fn verified(&self) -> bool {
        self.verified
    }
    fn levels_passed(&self) -> Option<i32> {
        self.levels_passed
    }
    fn levels_total(&self) -> Option<i32> {
        self.levels_total
    }
    fn updated_at_rfc3339(&self) -> String {
        self.updated_at.to_rfc3339()
    }
    fn try_parse_verification_result(&self) -> Option<EntityVerificationResult> {
        self.verification_result
            .clone()
            .and_then(|v| serde_json::from_value(v).ok())
    }
}

impl SpecRowLike for temper_store_turso::TursoSpecRow {
    fn verification_status(&self) -> &str {
        &self.verification_status
    }
    fn verified(&self) -> bool {
        self.verified
    }
    fn levels_passed(&self) -> Option<i32> {
        self.levels_passed
    }
    fn levels_total(&self) -> Option<i32> {
        self.levels_total
    }
    fn updated_at_rfc3339(&self) -> String {
        self.updated_at.clone()
    }
    fn try_parse_verification_result(&self) -> Option<EntityVerificationResult> {
        self.verification_result
            .as_ref()
            .and_then(|s| serde_json::from_str(s).ok())
    }
}

fn row_to_registry_status(row: &impl SpecRowLike) -> VerificationStatus {
    let status = row.verification_status().to_lowercase();
    match status.as_str() {
        "pending" => VerificationStatus::Pending,
        "running" => VerificationStatus::Running,
        _ => {
            // Full verification_result JSON → Completed (authoritative).
            if let Some(result) = row.try_parse_verification_result() {
                return VerificationStatus::Completed(result);
            }

            // No full result — build a synthetic summary and mark as Restored.
            let all_passed = status == "passed" || row.verified();
            let levels_passed = row
                .levels_passed()
                .unwrap_or(if all_passed { 1 } else { 0 })
                .max(0) as usize;
            let levels_total = row.levels_total().unwrap_or(levels_passed as i32).max(0) as usize;
            let levels = if levels_total > 0 {
                (0..levels_total)
                    .map(|idx| EntityLevelSummary {
                        level: format!("L{idx}"),
                        passed: idx < levels_passed,
                        summary: if idx < levels_passed {
                            "Restored from verification summary".to_string()
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
                    summary: format!("Restored status '{}'", row.verification_status()),
                    details: None,
                }]
            };
            VerificationStatus::Restored(EntityVerificationResult {
                all_passed,
                levels,
                warnings: Vec::new(),
                errors: Vec::new(),
                verified_at: row.updated_at_rfc3339(),
            })
        }
    }
}

/// Helper: populate registry from grouped spec rows.
fn restored_csdl_for_rows<R>(
    tenant: &str,
    rows: &[R],
    get_csdl: &impl Fn(&R) -> Option<String>,
) -> Result<Option<(CsdlDocument, String)>, String> {
    let mut seen = BTreeSet::new();
    let mut merged: Option<CsdlDocument> = None;

    for csdl_xml in rows.iter().filter_map(get_csdl) {
        let csdl_xml = csdl_xml.trim();
        if csdl_xml.is_empty() || !seen.insert(csdl_xml.to_string()) {
            continue;
        }

        let parsed = parse_csdl(csdl_xml)
            .map_err(|e| format!("Failed to parse restored CSDL for tenant '{tenant}': {e}"))?;
        merged = Some(match merged {
            Some(existing) => merge_csdl(&existing, &parsed),
            None => parsed,
        });
    }

    Ok(merged.map(|csdl| {
        let csdl_xml = emit_csdl_xml(&csdl);
        (csdl, csdl_xml)
    }))
}

fn populate_registry<R: SpecRowLike>(
    registry: &mut SpecRegistry,
    grouped: BTreeMap<String, Vec<R>>,
    constraints_by_tenant: &mut BTreeMap<String, String>,
    get_csdl: impl Fn(&R) -> Option<String>,
    get_ioa: impl Fn(&R) -> (String, String),
) -> Result<usize, String> {
    let mut restored_specs = 0usize;
    for (tenant, tenant_rows) in grouped {
        let Some((csdl, csdl_xml)) = restored_csdl_for_rows(&tenant, &tenant_rows, &get_csdl)?
        else {
            tracing::warn!(tenant = %tenant, "skipping restored tenant due to missing CSDL");
            continue;
        };

        let ioa_owned: Vec<(String, String)> = tenant_rows.iter().map(&get_ioa).collect();
        let ioa_pairs: Vec<(&str, &str)> = ioa_owned
            .iter()
            .map(|(entity_type, ioa)| (entity_type.as_str(), ioa.as_str()))
            .collect();

        let cross_invariants_toml = constraints_by_tenant.remove(&tenant);
        registry
            .try_register_tenant_with_reactions_and_constraints(
                tenant.as_str(),
                csdl,
                csdl_xml,
                &ioa_pairs,
                Vec::new(),
                cross_invariants_toml,
                false,
            )
            .map_err(|e| format!("Failed to restore tenant '{tenant}' into registry: {e}"))?;
        let tenant_id = TenantId::new(&tenant);
        for row in &tenant_rows {
            registry.set_verification_status(
                &tenant_id,
                &get_ioa(row).0,
                row_to_registry_status(row),
            );
            restored_specs += 1;
        }
    }
    Ok(restored_specs)
}

/// Restore a [`SpecRegistry`] from Postgres.
pub async fn restore_registry_from_postgres(
    registry: &mut SpecRegistry,
    pool: &sqlx::PgPool,
) -> Result<usize, String> {
    let rows: Vec<PersistedSpecRow> = sqlx::query_as(
        "SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, \
                levels_passed, levels_total, verification_result, updated_at \
         FROM specs \
         ORDER BY tenant, entity_type",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to read specs from Postgres: {e}"))?;

    #[derive(sqlx::FromRow)]
    struct ConstraintRow {
        tenant: String,
        cross_invariants_toml: String,
    }

    let constraints_rows: Vec<ConstraintRow> = sqlx::query_as(
        "SELECT tenant, cross_invariants_toml \
         FROM tenant_constraints \
         ORDER BY tenant",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to read tenant constraints from Postgres: {e}"))?;

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

    populate_registry(
        registry,
        grouped,
        &mut constraints_by_tenant,
        |rows| rows.csdl_xml.clone(),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    )
}

/// Restore a [`SpecRegistry`] from Turso.
pub async fn restore_registry_from_turso(
    registry: &mut SpecRegistry,
    turso: &TursoEventStore,
) -> Result<usize, String> {
    // GC uncommitted specs left behind by interrupted install_os_app writes.
    match turso.delete_uncommitted_specs().await {
        Ok(0) => {}
        Ok(n) => tracing::info!("deleted {n} uncommitted specs during startup recovery"),
        Err(e) => tracing::warn!("failed to delete uncommitted specs: {e}"),
    }
    let rows = turso
        .load_specs()
        .await
        .map_err(|e| format!("Failed to read specs from Turso: {e}"))?;
    let constraints_rows = turso
        .load_tenant_constraints()
        .await
        .map_err(|e| format!("Failed to read tenant constraints from Turso: {e}"))?;

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

    populate_registry(
        registry,
        grouped,
        &mut constraints_by_tenant,
        |rows| rows.csdl_xml.clone(),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    )
}

/// Restore a [`SpecRegistry`] from a [`PlatformStore`] (trait-based).
///
/// This is the production code path for restoring specs from any platform
/// store backend (Turso, Sim, etc.). Used by both the CLI bootstrap and
/// the DST harness — ensuring simulation runs identical code.
///
/// Unlike [`restore_registry_from_turso`], this does not restore verification
/// status or cross-entity constraints (the `PlatformStore` trait returns simpler
/// `SpecRow` types). Specs are registered with `Pending` verification status.
pub async fn restore_registry_from_platform_store(
    registry: &mut SpecRegistry,
    store: &dyn crate::platform_store::PlatformStore,
) -> Result<usize, String> {
    match store.delete_uncommitted_specs().await {
        Ok(0) => {}
        Ok(n) => tracing::info!("deleted {n} uncommitted specs during startup recovery"),
        Err(e) => tracing::warn!("failed to delete uncommitted specs: {e}"),
    }
    let rows = store
        .load_specs()
        .await
        .map_err(|e| format!("Failed to read specs from platform store: {e}"))?;

    if rows.is_empty() {
        return Ok(0);
    }

    // Group by tenant.
    let mut grouped: BTreeMap<String, Vec<crate::platform_store::SpecRow>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.tenant.clone()).or_default().push(row);
    }

    let mut restored_specs = 0usize;
    // Track specs that failed to register — these are orphans to reconcile.
    let mut orphaned_specs: Vec<(String, String)> = Vec::new();

    for (tenant, tenant_rows) in &grouped {
        let (csdl, csdl_xml) = match restored_csdl_for_rows(tenant, tenant_rows, &|r| {
            r.csdl_xml.clone()
        }) {
            Ok(Some(restored)) => restored,
            Ok(None) => {
                tracing::warn!(tenant = %tenant, "reconciling orphaned specs for tenant with missing CSDL");
                for row in tenant_rows {
                    orphaned_specs.push((row.tenant.clone(), row.entity_type.clone()));
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(tenant = %tenant, "reconciling orphaned specs for tenant with invalid CSDL: {e}");
                for row in tenant_rows {
                    orphaned_specs.push((row.tenant.clone(), row.entity_type.clone()));
                }
                continue;
            }
        };

        let ioa_pairs: Vec<(&str, &str)> = tenant_rows
            .iter()
            .map(|r| (r.entity_type.as_str(), r.ioa_source.as_str()))
            .collect();

        match registry.try_register_tenant_with_reactions_and_constraints(
            tenant.as_str(),
            csdl,
            csdl_xml,
            &ioa_pairs,
            Vec::new(),
            None,
            false,
        ) {
            Ok(()) => {
                restored_specs += tenant_rows.len();
            }
            Err(e) => {
                tracing::warn!(tenant = %tenant, "reconciling orphaned specs for tenant that failed registration: {e}");
                for row in tenant_rows {
                    orphaned_specs.push((row.tenant.clone(), row.entity_type.clone()));
                }
            }
        }
    }

    // Reconciliation: delete orphaned specs from the store so P1 holds
    // (every spec in store has a matching registry entry).
    for (tenant, entity_type) in &orphaned_specs {
        if let Err(e) = store.delete_spec(tenant, entity_type).await {
            tracing::warn!(
                tenant = %tenant,
                entity_type = %entity_type,
                "best-effort orphan cleanup failed: {e}"
            );
        }
    }

    Ok(restored_specs)
}

#[cfg(test)]
#[path = "registry_bootstrap_test.rs"]
mod tests;
