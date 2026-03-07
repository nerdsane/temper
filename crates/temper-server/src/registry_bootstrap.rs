//! Persistence bootstrap — restoring a [`SpecRegistry`] from storage backends.
//!
//! Centralizes the logic for reading persisted specs from Postgres or Turso and
//! populating a `SpecRegistry` with tenant registrations and verification status.
//! This keeps storage-specific row translation out of the CLI layer.

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
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
    fn verification_status(&self) -> &str { &self.verification_status }
    fn verified(&self) -> bool { self.verified }
    fn levels_passed(&self) -> Option<i32> { self.levels_passed }
    fn levels_total(&self) -> Option<i32> { self.levels_total }
    fn updated_at_rfc3339(&self) -> String { self.updated_at.to_rfc3339() }
    fn try_parse_verification_result(&self) -> Option<EntityVerificationResult> {
        self.verification_result
            .clone()
            .and_then(|v| serde_json::from_value(v).ok())
    }
}

impl SpecRowLike for temper_store_turso::TursoSpecRow {
    fn verification_status(&self) -> &str { &self.verification_status }
    fn verified(&self) -> bool { self.verified }
    fn levels_passed(&self) -> Option<i32> { self.levels_passed }
    fn levels_total(&self) -> Option<i32> { self.levels_total }
    fn updated_at_rfc3339(&self) -> String { self.updated_at.clone() }
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
fn populate_registry<R: SpecRowLike>(
    registry: &mut SpecRegistry,
    grouped: BTreeMap<String, Vec<R>>,
    constraints_by_tenant: &mut BTreeMap<String, String>,
    get_csdl: impl Fn(&[R]) -> String,
    get_ioa: impl Fn(&R) -> (String, String),
) -> Result<usize, String> {
    let mut restored_specs = 0usize;
    for (tenant, tenant_rows) in grouped {
        let csdl_xml = get_csdl(&tenant_rows);
        if csdl_xml.trim().is_empty() {
            tracing::warn!(tenant = %tenant, "skipping restored tenant due to missing CSDL");
            continue;
        }
        let csdl = parse_csdl(&csdl_xml)
            .map_err(|e| format!("Failed to parse restored CSDL for tenant '{tenant}': {e}"))?;

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
        |rows| rows.iter().find_map(|r| r.csdl_xml.clone()).unwrap_or_default(),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    )
}

/// Restore a [`SpecRegistry`] from Turso.
pub async fn restore_registry_from_turso(
    registry: &mut SpecRegistry,
    turso: &TursoEventStore,
) -> Result<usize, String> {
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
        |rows| rows.iter().find_map(|r| r.csdl_xml.clone()).unwrap_or_default(),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock implementation of SpecRowLike for testing row_to_registry_status.
    struct MockRow {
        status: String,
        verified: bool,
        levels_passed: Option<i32>,
        levels_total: Option<i32>,
        updated_at: String,
        verification_result: Option<EntityVerificationResult>,
    }

    impl SpecRowLike for MockRow {
        fn verification_status(&self) -> &str { &self.status }
        fn verified(&self) -> bool { self.verified }
        fn levels_passed(&self) -> Option<i32> { self.levels_passed }
        fn levels_total(&self) -> Option<i32> { self.levels_total }
        fn updated_at_rfc3339(&self) -> String { self.updated_at.clone() }
        fn try_parse_verification_result(&self) -> Option<EntityVerificationResult> {
            self.verification_result.clone()
        }
    }

    #[test]
    fn row_to_registry_status_pending() {
        let status = row_to_registry_status(&MockRow { status: "pending".into(), verified: false, levels_passed: None, levels_total: None, updated_at: "2024-01-01T00:00:00Z".into(), verification_result: None });
        assert!(matches!(status, VerificationStatus::Pending));
    }

    #[test]
    fn row_to_registry_status_running() {
        let status = row_to_registry_status(&MockRow { status: "running".into(), verified: false, levels_passed: None, levels_total: None, updated_at: "2024-01-01T00:00:00Z".into(), verification_result: None });
        assert!(matches!(status, VerificationStatus::Running));
    }

    #[test]
    fn row_to_registry_status_passed() {
        let status = row_to_registry_status(&MockRow { status: "passed".into(), verified: true, levels_passed: Some(3), levels_total: Some(3), updated_at: "2024-01-01T00:00:00Z".into(), verification_result: None });
        match status {
            VerificationStatus::Restored(result) => assert!(result.all_passed),
            other => panic!("Expected Restored, got {other:?}"),
        }
    }

    #[test]
    fn row_to_registry_status_failed() {
        let status = row_to_registry_status(&MockRow { status: "failed".into(), verified: false, levels_passed: Some(1), levels_total: Some(3), updated_at: "2024-01-01T00:00:00Z".into(), verification_result: None });
        match status {
            VerificationStatus::Restored(result) => {
                assert!(!result.all_passed);
                assert_eq!(result.levels.len(), 3);
                assert!(result.levels[0].passed);
                assert!(!result.levels[1].passed);
            }
            other => panic!("Expected Restored, got {other:?}"),
        }
    }
}
