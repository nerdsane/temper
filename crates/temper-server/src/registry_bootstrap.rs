//! Persistence bootstrap — restoring a [`SpecRegistry`] from storage backends.
//!
//! Centralizes the logic for reading persisted specs from Postgres or Turso and
//! populating a `SpecRegistry` with tenant registrations and verification status.
//! This keeps storage-specific row translation out of the CLI layer.

#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{CsdlDocument, emit_csdl_xml, merge_csdl, parse_csdl};
use temper_store_turso::TursoEventStore;

use crate::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};

mod platform_store;
mod stored;
pub use platform_store::restore_registry_from_platform_store;
pub use stored::{
    StoredTenant, StoredTenants, read_stored_tenants_from_postgres, read_stored_tenants_from_turso,
    register_stored_tenant,
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

/// Restore every tenant in `grouped` into `registry`
/// ([`register_tenant_rows`] for each).
#[cfg(test)]
fn populate_registry<R: SpecRowLike>(
    registry: &mut SpecRegistry,
    grouped: BTreeMap<String, Vec<R>>,
    constraints_by_tenant: &mut BTreeMap<String, String>,
    get_csdl: impl Fn(&R) -> Option<String>,
    get_ioa: impl Fn(&R) -> (String, String),
) -> usize {
    grouped
        .into_iter()
        .map(|(tenant, tenant_rows)| {
            let constraints = constraints_by_tenant.remove(&tenant);
            register_tenant_rows(
                registry,
                tenant,
                tenant_rows,
                constraints,
                &get_csdl,
                &get_ioa,
            )
        })
        .sum()
}

/// Register one stored tenant's spec rows and constraints into `registry`,
/// returning how many specs it restored.
///
/// The tenant's rows belong to it alone, so a tenant whose stored CSDL or
/// constraints do not load is logged, marked unavailable in the registry and
/// left out; every other tenant is still restored. Its rows stay in the store
/// untouched, so once they are fixed the next start restores it. Only
/// store-level read failures, handled by the callers, stop a restore.
fn register_tenant_rows<R: SpecRowLike>(
    registry: &mut SpecRegistry,
    tenant: String,
    tenant_rows: Vec<R>,
    constraints: Option<String>,
    get_csdl: &impl Fn(&R) -> Option<String>,
    get_ioa: &impl Fn(&R) -> (String, String),
) -> usize {
    let mut restored_specs = 0usize;
    let (csdl, csdl_xml) = match restored_csdl_for_rows(&tenant, &tenant_rows, get_csdl) {
        Ok(Some(restored)) => restored,
        Ok(None) => {
            tracing::warn!(tenant = %tenant, "skipping restored tenant due to missing CSDL");
            return 0;
        }
        Err(reason) => {
            mark_unrestorable(registry, &tenant, reason);
            return 0;
        }
    };

    // A stored spec in the old predicate syntax is converted in memory; one
    // that still cannot be read disables only its own entity type, not
    // the tenant or the server.
    let ioa_owned: Vec<(String, String)> = tenant_rows
        .iter()
        .map(get_ioa)
        .filter_map(
            |(entity_type, ioa)| match read_stored_spec(&tenant, &entity_type, &ioa) {
                StoredSpec::Current => Some((entity_type, ioa)),
                StoredSpec::Converted(source) => Some((entity_type, source)),
                StoredSpec::Unreadable => None,
            },
        )
        .collect();
    let ioa_pairs: Vec<(&str, &str)> = ioa_owned
        .iter()
        .map(|(entity_type, ioa)| (entity_type.as_str(), ioa.as_str()))
        .collect();

    if let Err(error) = registry.try_register_tenant_with_constraints(
        tenant.as_str(),
        csdl,
        csdl_xml,
        &ioa_pairs,
        constraints,
        false,
    ) {
        mark_unrestorable(
            registry,
            &tenant,
            format!("Failed to restore tenant '{tenant}' into registry: {error}"),
        );
        return 0;
    }
    let tenant_id = TenantId::new(&tenant);
    for row in &tenant_rows {
        let entity_type = get_ioa(row).0;
        if !ioa_owned
            .iter()
            .any(|(restored, _)| *restored == entity_type)
        {
            continue;
        }
        registry.set_verification_status(&tenant_id, &entity_type, row_to_registry_status(row));
        restored_specs += 1;
    }
    restored_specs
}

/// Log a tenant whose stored rows do not load and mark it unavailable.
fn mark_unrestorable(registry: &mut SpecRegistry, tenant: &str, reason: String) {
    tracing::error!(
        tenant,
        %reason,
        "stored specs for this tenant do not load; the tenant is unavailable until they are fixed, every other tenant is restored"
    );
    registry.mark_tenant_unavailable(tenant, reason);
}

/// Restore a [`SpecRegistry`] from Postgres.
pub async fn restore_registry_from_postgres(
    registry: &mut SpecRegistry,
    pool: &sqlx::PgPool,
) -> Result<usize, String> {
    let store = temper_store_postgres::PostgresEventStore::new(pool.clone());
    restore_registry_from_postgres_store(registry, &store).await
}

/// Restore a registry using the schema selected by the PostgreSQL store.
pub async fn restore_registry_from_postgres_store(
    registry: &mut SpecRegistry,
    store: &temper_store_postgres::PostgresEventStore,
) -> Result<usize, String> {
    let stored = read_stored_tenants_from_postgres(store).await?;
    Ok(register_stored_tenants(registry, stored))
}

/// Restore a [`SpecRegistry`] from Turso.
pub async fn restore_registry_from_turso(
    registry: &mut SpecRegistry,
    turso: &TursoEventStore,
) -> Result<usize, String> {
    let stored = read_stored_tenants_from_turso(turso).await?;
    Ok(register_stored_tenants(registry, stored))
}

/// Register every one of `stored` into `registry`, returning how many specs
/// were restored.
fn register_stored_tenants(registry: &mut SpecRegistry, stored: StoredTenants) -> usize {
    stored
        .into_iter()
        .map(|(tenant, stored)| register_stored_tenant(registry, tenant, stored))
        .sum()
}

/// How a persisted spec reads under this kernel.
enum StoredSpec {
    /// Parses as stored.
    Current,
    /// Was in the old predicate syntax; this is the converted source.
    Converted(String),
    /// Neither parses nor converts.
    Unreadable,
}

fn read_stored_spec(tenant: &str, entity_type: &str, ioa_source: &str) -> StoredSpec {
    let Err(parse_error) = temper_spec::automaton::parse_automaton(ioa_source) else {
        return StoredSpec::Current;
    };
    match temper_spec::automaton::legacy::migrate_source(ioa_source) {
        Ok(migration) => {
            tracing::info!(
                tenant,
                entity_type,
                notes = migration.notes.len(),
                "converted stored spec to the current syntax"
            );
            StoredSpec::Converted(migration.source)
        }
        Err(convert_error) => {
            tracing::error!(
                tenant,
                entity_type,
                %parse_error,
                %convert_error,
                "stored spec does not parse and cannot be converted; its entity type stays unavailable until the spec is fixed"
            );
            StoredSpec::Unreadable
        }
    }
}

#[cfg(test)]
#[path = "registry_bootstrap_test.rs"]
mod tests;
