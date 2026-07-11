//! Persistence bootstrap — restoring a [`SpecRegistry`] from storage backends.
//!
//! Centralizes the logic for reading persisted specs from Postgres or Turso and
//! populating a `SpecRegistry` with tenant registrations and verification status.
//! This keeps storage-specific row translation out of the CLI layer.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{CsdlDocument, emit_csdl_xml, merge_csdl, parse_csdl};
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::TursoEventStore;

use crate::platform_store::{
    PlatformStore, RegistryQuarantineRecord, RegistryQuarantineResolution,
    RegistryQuarantineUpsert, RegistrySourceSnapshot, SpecRow, TenantConstraintRow,
};
use crate::registry::{
    EntityLevelSummary, EntityVerificationResult, RegistryQuarantineFailure,
    RegistryQuarantineReason, RegistryQuarantineSource, RegistryRestoreHealth,
    RegistryTenantQuarantine, SpecRegistry, VerificationStatus,
};

const QUARANTINE_DETAIL_BUDGET_BYTES: usize = 512;
pub(crate) const REGISTRY_QUARANTINE_ENTITY_BUDGET: usize = 256;

/// Common accessors for spec rows from different storage backends.
trait SpecRowLike {
    fn spec_version(&self) -> i64;
    fn verification_status(&self) -> &str;
    fn verified(&self) -> bool;
    fn levels_passed(&self) -> Option<i32>;
    fn levels_total(&self) -> Option<i32>;
    fn updated_at_rfc3339(&self) -> String;
    fn try_parse_verification_result(&self) -> Option<EntityVerificationResult>;
}

impl SpecRowLike for SpecRow {
    fn spec_version(&self) -> i64 {
        self.version
    }
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

/// Parse and merge every distinct CSDL fragment persisted for one tenant.
///
/// Returns the merged document plus its re-emitted XML, or `Ok(None)` when the
/// tenant has no non-empty CSDL. An unparsable fragment is an `Err` the caller
/// turns into a per-tenant quarantine.
struct RestoredCsdlError {
    source: String,
    detail: String,
}

fn restored_csdl_for_rows<R>(
    tenant: &str,
    rows: &[R],
    get_csdl: &impl Fn(&R) -> Option<String>,
) -> Result<Option<(CsdlDocument, String)>, RestoredCsdlError> {
    let mut seen = BTreeSet::new();
    let mut merged: Option<CsdlDocument> = None;

    for csdl_xml in rows.iter().filter_map(get_csdl) {
        let csdl_xml = csdl_xml.trim();
        if csdl_xml.is_empty() || !seen.insert(csdl_xml.to_string()) {
            continue;
        }

        let parsed = parse_csdl(csdl_xml).map_err(|error| RestoredCsdlError {
            source: csdl_xml.to_string(),
            detail: format!("Failed to parse restored CSDL for tenant '{tenant}': {error}"),
        })?;
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

fn bounded_quarantine_detail(error: &str) -> String {
    let normalized = error.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() <= QUARANTINE_DETAIL_BUDGET_BYTES {
        return normalized;
    }
    let mut end = QUARANTINE_DETAIL_BUDGET_BYTES - '…'.len_utf8();
    while !normalized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &normalized[..end])
}

fn number_after(text: &str, marker: &str) -> Option<i64> {
    let start = text.to_ascii_lowercase().find(marker)? + marker.len();
    let digits = text[start..]
        .chars()
        .skip_while(|character| !character.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn source_position(error: &str) -> (Option<i64>, Option<i64>) {
    (
        number_after(error, "line"),
        number_after(error, "column").or_else(|| number_after(error, "col")),
    )
}

fn quarantine_failure(
    version: i64,
    constraint_version: Option<i64>,
    reason: RegistryQuarantineReason,
    source_kind: RegistryQuarantineSource,
    error: &str,
) -> RegistryQuarantineFailure {
    let (source_line, source_column) = source_position(error);
    RegistryQuarantineFailure {
        spec_version: version,
        constraint_version,
        reason,
        source_kind,
        source_line,
        source_column,
        acknowledged: false,
        detail: bounded_quarantine_detail(error),
    }
}

/// Fault-isolating per-tenant restore core shared by every backend.
///
/// For each tenant this parses+merges the persisted CSDL and registers the
/// tenant. A single tenant failing for any reason — missing CSDL, a CSDL parse
/// error, or a registration error — is logged and **quarantined**; the loop
/// continues so the remaining tenants still restore. One corrupt persisted row
/// can therefore never abort boot for the healthy tenants.
///
/// Every restore path funnels through this one function, so the DST harness
/// exercises exactly the per-tenant isolation logic the live server ships.
fn restore_grouped_specs<R: SpecRowLike>(
    registry: &mut SpecRegistry,
    grouped: BTreeMap<String, Vec<R>>,
    constraints_by_tenant: &mut BTreeMap<String, (String, i64)>,
    get_csdl: impl Fn(&R) -> Option<String>,
    get_ioa: impl Fn(&R) -> (String, String),
    mut on_registered: impl FnMut(&mut SpecRegistry, &TenantId, &R, &str),
) -> RegistryRestoreHealth {
    let mut report = RegistryRestoreHealth::default();

    for (tenant, tenant_rows) in grouped {
        let constraint_version = constraints_by_tenant
            .get(&tenant)
            .map(|(_, version)| *version);
        let ioa_owned: Vec<(String, String)> = tenant_rows.iter().map(&get_ioa).collect();
        let entity_failures = |reason, source_kind, detail: &str| {
            tenant_rows
                .iter()
                .zip(ioa_owned.iter())
                .map(|(row, (entity_type, _))| {
                    (
                        entity_type.clone(),
                        quarantine_failure(
                            row.spec_version(),
                            constraint_version,
                            reason,
                            source_kind,
                            detail,
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };

        // Parse + merge this tenant's CSDL. A missing or unparsable CSDL
        // quarantines only this tenant — the rest still restore.
        let (csdl, csdl_xml) = match restored_csdl_for_rows(&tenant, &tenant_rows, &get_csdl) {
            Ok(Some(restored)) => restored,
            Ok(None) => {
                tracing::warn!(tenant = %tenant, "quarantining tenant during restore: missing CSDL");
                let detail = format!("tenant '{tenant}' has no non-empty persisted CSDL");
                report.quarantined_tenants.insert(
                    tenant,
                    RegistryTenantQuarantine {
                        entity_failures: entity_failures(
                            RegistryQuarantineReason::MissingCsdl,
                            RegistryQuarantineSource::Csdl,
                            &detail,
                        ),
                    },
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(tenant = %tenant, error = %error.detail, "quarantining tenant during restore: invalid CSDL");
                let failures = tenant_rows
                    .iter()
                    .zip(ioa_owned.iter())
                    .map(|(row, (entity_type, _))| {
                        let is_direct_source = get_csdl(row)
                            .is_some_and(|source| source.trim() == error.source.as_str());
                        let (source_kind, detail) = if is_direct_source {
                            (RegistryQuarantineSource::Csdl, error.detail.clone())
                        } else {
                            (
                                RegistryQuarantineSource::Registration,
                                "tenant activation withheld because a sibling CSDL fragment failed to parse"
                                    .to_string(),
                            )
                        };
                        (
                            entity_type.clone(),
                            quarantine_failure(
                                row.spec_version(),
                                constraint_version,
                                RegistryQuarantineReason::InvalidCsdl,
                                source_kind,
                                &detail,
                            ),
                        )
                    })
                    .collect();
                report.quarantined_tenants.insert(
                    tenant,
                    RegistryTenantQuarantine {
                        entity_failures: failures,
                    },
                );
                continue;
            }
        };

        let ioa_pairs: Vec<(&str, &str)> = ioa_owned
            .iter()
            .map(|(entity_type, ioa)| (entity_type.as_str(), ioa.as_str()))
            .collect();

        let cross_invariants_toml = constraints_by_tenant
            .remove(&tenant)
            .map(|(source, _version)| source);
        match registry.try_register_tenant_with_reactions_and_constraints(
            tenant.as_str(),
            csdl,
            csdl_xml,
            &ioa_pairs,
            Vec::new(),
            cross_invariants_toml,
            false,
        ) {
            Ok(()) => {
                let tenant_id = TenantId::new(&tenant);
                for (row, (entity_type, _ioa)) in tenant_rows.iter().zip(ioa_owned.iter()) {
                    on_registered(registry, &tenant_id, row, entity_type);
                }
                report.restored_specs = report.restored_specs.saturating_add(ioa_owned.len());
            }
            Err(e) => {
                tracing::warn!(tenant = %tenant, error = %e, "quarantining tenant during restore: registration failed");
                let failures = tenant_rows
                    .iter()
                    .zip(ioa_owned.iter())
                    .map(|(row, (entity_type, _))| {
                        let (source_kind, detail) = match &e {
                            crate::registry::RegistryError::CrossInvariantParse { .. } => {
                                (RegistryQuarantineSource::CrossInvariants, e.to_string())
                            }
                            crate::registry::RegistryError::IoaParse {
                                entity_type: failed_entity,
                                ..
                            } if failed_entity == entity_type => {
                                (RegistryQuarantineSource::Ioa, e.to_string())
                            }
                            crate::registry::RegistryError::IoaParse {
                                entity_type: failed_entity,
                                ..
                            } => (
                                RegistryQuarantineSource::Registration,
                                format!(
                                    "tenant activation withheld because sibling entity '{failed_entity}' failed IOA registration"
                                ),
                            ),
                        };
                        (
                            entity_type.clone(),
                            quarantine_failure(
                                row.spec_version(),
                                constraint_version,
                                RegistryQuarantineReason::RegistrationFailed,
                                source_kind,
                                &detail,
                            ),
                        )
                    })
                    .collect();
                report.quarantined_tenants.insert(
                    tenant,
                    RegistryTenantQuarantine {
                        entity_failures: failures,
                    },
                );
            }
        }
    }

    report
}

/// Restore the Postgres/Turso row types, which additionally carry persisted
/// verification status. Delegates the per-tenant isolation to
/// [`restore_grouped_specs`] and layers verification-status restoration on top.
fn populate_registry<R: SpecRowLike>(
    registry: &mut SpecRegistry,
    grouped: BTreeMap<String, Vec<R>>,
    constraints_by_tenant: &mut BTreeMap<String, (String, i64)>,
    get_csdl: impl Fn(&R) -> Option<String>,
    get_ioa: impl Fn(&R) -> (String, String),
) -> RegistryRestoreHealth {
    restore_grouped_specs(
        registry,
        grouped,
        constraints_by_tenant,
        get_csdl,
        get_ioa,
        |registry, tenant_id, row, entity_type| {
            registry.set_verification_status(tenant_id, entity_type, row_to_registry_status(row));
        },
    )
}

mod operations;
pub use operations::{
    RegistryRetryError, restore_registry_from_platform_store, restore_registry_from_postgres,
    restore_registry_from_turso, retry_registry_tenant,
};

#[cfg(test)]
#[path = "registry_bootstrap_test.rs"]
mod tests;
