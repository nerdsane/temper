//! Version-bound, replica-safe registry quarantine repair.

use super::*;

const RETRY_IDENTITY_BUDGET: usize = REGISTRY_QUARANTINE_ENTITY_BUDGET * 2;

#[derive(Debug, PartialEq, Eq)]
struct SpecSourceSnapshot<'a> {
    tenant: &'a str,
    entity_type: &'a str,
    ioa_source: &'a str,
    csdl_xml: Option<&'a str>,
    content_hash: &'a str,
    version: i64,
    committed: bool,
}

fn spec_source_snapshot(rows: &[SpecRow]) -> Vec<SpecSourceSnapshot<'_>> {
    rows.iter()
        .map(|row| SpecSourceSnapshot {
            tenant: &row.tenant,
            entity_type: &row.entity_type,
            ioa_source: &row.ioa_source,
            csdl_xml: row.csdl_xml.as_deref(),
            content_hash: &row.content_hash,
            version: row.version,
            committed: row.committed,
        })
        .collect()
}

/// Typed failure returned by the authenticated registry repair workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryRetryError {
    /// The tenant has no committed source or no local/durable quarantine identity.
    NotFound(String),
    /// Source/quarantine versions changed during the compare-and-set workflow.
    Conflict(String),
    /// A durable backend operation failed.
    Storage(String),
}

impl std::fmt::Display for RegistryRetryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message) | Self::Conflict(message) | Self::Storage(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for RegistryRetryError {}

type QuarantineIdentity = (String, i64, Option<i64>);

fn local_quarantine_identities(
    registry: &std::sync::Arc<std::sync::RwLock<SpecRegistry>>,
    tenant: &str,
) -> Vec<QuarantineIdentity> {
    registry
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .restore_health()
        .quarantined_tenants
        .get(tenant)
        .map(|quarantine| {
            quarantine
                .entity_failures
                .iter()
                .map(|(entity_type, failure)| {
                    (
                        entity_type.clone(),
                        failure.spec_version,
                        failure.constraint_version,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn load_tenant_specs(
    store: &dyn PlatformStore,
    tenant: &str,
) -> Result<Vec<SpecRow>, RegistryRetryError> {
    let mut rows = store
        .load_specs()
        .await
        .map_err(|error| {
            RegistryRetryError::Storage(format!("Failed to read specs for retry: {error}"))
        })?
        .into_iter()
        .filter(|row| row.tenant == tenant)
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.entity_type.cmp(&right.entity_type));
    Ok(rows)
}

async fn load_tenant_constraints(
    store: &dyn PlatformStore,
    tenant: &str,
) -> Result<Vec<TenantConstraintRow>, RegistryRetryError> {
    let rows = store
        .load_tenant_constraints()
        .await
        .map_err(|error| {
            RegistryRetryError::Storage(format!("Failed to read constraints for retry: {error}"))
        })?
        .into_iter()
        .filter(|row| row.tenant == tenant)
        .collect::<Vec<_>>();
    if rows.len() > 1 {
        return Err(RegistryRetryError::Conflict(format!(
            "Tenant '{tenant}' has multiple persisted constraint rows"
        )));
    }
    Ok(rows)
}

fn retry_identities(
    local: Vec<QuarantineIdentity>,
    durable: &[RegistryQuarantineRecord],
    tenant: &str,
) -> Result<Vec<QuarantineIdentity>, RegistryRetryError> {
    let mut identities = local.into_iter().collect::<BTreeSet<_>>();
    identities.extend(durable.iter().map(|record| {
        (
            record.entity_type.clone(),
            record.spec_version,
            record.constraint_version,
        )
    }));
    if identities.is_empty() {
        return Err(RegistryRetryError::NotFound(format!(
            "No local or durable registry quarantine found for tenant '{tenant}'"
        )));
    }
    if identities.len() > RETRY_IDENTITY_BUDGET {
        return Err(RegistryRetryError::Conflict(format!(
            "Tenant '{tenant}' exceeds the {RETRY_IDENTITY_BUDGET}-identity registry repair budget"
        )));
    }
    Ok(identities.into_iter().collect())
}

/// Re-run restore for one tenant after an operator repairs its committed source.
///
/// An already-resolved exact quarantine identity is accepted so a stale replica
/// can validate and activate the same complete source snapshot. A newer active
/// identity or any source-set insertion/removal makes the durable CAS fail.
pub async fn retry_registry_tenant(
    registry: &std::sync::Arc<std::sync::RwLock<SpecRegistry>>,
    store: &dyn PlatformStore,
    tenant: &str,
    requested_entity_type: &str,
) -> Result<RegistryRestoreHealth, RegistryRetryError> {
    let tenant_id = TenantId::new(tenant);
    let live_source_before = registry
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .tenant_source_snapshot(&tenant_id);
    let local_identities = local_quarantine_identities(registry, tenant);
    if local_identities.len() > REGISTRY_QUARANTINE_ENTITY_BUDGET {
        return Err(RegistryRetryError::Conflict(format!(
            "Tenant '{tenant}' exceeds the {REGISTRY_QUARANTINE_ENTITY_BUDGET}-entity registry repair budget"
        )));
    }
    let rows = load_tenant_specs(store, tenant).await?;
    if rows.is_empty() {
        return Err(RegistryRetryError::NotFound(format!(
            "No committed persisted specs found for tenant '{tenant}'"
        )));
    }
    if rows.len() > REGISTRY_QUARANTINE_ENTITY_BUDGET {
        return Err(RegistryRetryError::Conflict(format!(
            "Tenant '{tenant}' exceeds the {REGISTRY_QUARANTINE_ENTITY_BUDGET}-entity registry repair budget"
        )));
    }
    let active = store
        .load_registry_restore_quarantines_for_tenant(tenant, REGISTRY_QUARANTINE_ENTITY_BUDGET + 1)
        .await
        .map_err(|error| {
            RegistryRetryError::Storage(format!("Failed to read active retry quarantine: {error}"))
        })?;
    if active.len() > REGISTRY_QUARANTINE_ENTITY_BUDGET {
        return Err(RegistryRetryError::Conflict(format!(
            "Tenant '{tenant}' exceeds the {REGISTRY_QUARANTINE_ENTITY_BUDGET}-entity registry repair budget"
        )));
    }
    let identities = retry_identities(local_identities, &active, tenant)?;
    if !identities
        .iter()
        .any(|(entity_type, _, _)| entity_type == requested_entity_type)
    {
        return Err(RegistryRetryError::NotFound(format!(
            "No local or durable registry quarantine found for tenant '{tenant}', entity '{requested_entity_type}'"
        )));
    }
    let constraint_rows = load_tenant_constraints(store, tenant).await?;
    let source = RegistrySourceSnapshot::from_rows(&rows, &constraint_rows)
        .map_err(RegistryRetryError::Conflict)?;
    let mut constraints = constraint_rows
        .iter()
        .map(|row| (row.tenant.clone(), (row.source.clone(), row.version)))
        .collect::<BTreeMap<_, _>>();
    let activation_constraints = constraints.clone();
    let grouped = BTreeMap::from([(tenant.to_string(), rows.clone())]);
    let mut scratch = SpecRegistry::new();
    let report = populate_registry(
        &mut scratch,
        grouped,
        &mut constraints,
        |row| row.csdl_xml.clone(),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    );

    if !report.is_healthy() {
        return persist_failed_retry(registry, store, tenant, &source, report).await;
    }

    let latest = load_tenant_specs(store, tenant).await?;
    let latest_constraints = load_tenant_constraints(store, tenant).await?;
    if spec_source_snapshot(&latest) != spec_source_snapshot(&rows)
        || latest_constraints != constraint_rows
    {
        return Err(RegistryRetryError::Conflict(format!(
            "Committed source for tenant '{tenant}' changed while retry validation was running"
        )));
    }

    let resolutions = identities
        .iter()
        .map(
            |(entity_type, version, constraint_version)| RegistryQuarantineResolution {
                tenant,
                entity_type,
                quarantined_version: *version,
                quarantined_constraint_version: *constraint_version,
            },
        )
        .collect::<Vec<_>>();
    let resolved = store
        .resolve_registry_restore_quarantines(&source, &resolutions)
        .await
        .map_err(|error| {
            RegistryRetryError::Storage(format!("Failed to resolve repaired quarantine: {error}"))
        })?;
    if !resolved {
        return Err(RegistryRetryError::Conflict(format!(
            "Quarantine or committed source set for tenant '{tenant}' changed during repair"
        )));
    }

    Ok(activate_validated_retry(
        registry,
        tenant,
        &tenant_id,
        live_source_before,
        rows,
        activation_constraints,
        report,
    ))
}

fn activate_validated_retry(
    registry: &std::sync::Arc<std::sync::RwLock<SpecRegistry>>,
    tenant: &str,
    tenant_id: &TenantId,
    live_source_before: Option<crate::registry::RegistryTenantSourceSnapshot>,
    rows: Vec<SpecRow>,
    mut constraints: BTreeMap<String, (String, i64)>,
    scratch_report: RegistryRestoreHealth,
) -> RegistryRestoreHealth {
    let mut live_registry = registry.write().unwrap_or_else(|error| error.into_inner());
    if live_registry.tenant_source_snapshot(tenant_id) != live_source_before {
        // Durable CAS is the cross-process linearization point. This guard is
        // narrower: a same-process mutation that registered newer source while
        // retry awaited storage must not be overwritten by the older validated
        // candidate. The newer registry is already parse-valid, so only stale
        // quarantine health needs reconciliation.
        tracing::warn!(
            tenant,
            "registry advanced during quarantine retry; preserving newer live source"
        );
        live_registry.replace_tenant_restore_quarantine(tenant, None);
        return scratch_report;
    }

    let grouped = BTreeMap::from([(tenant.to_string(), rows)]);
    let live_report = populate_registry(
        &mut live_registry,
        grouped,
        &mut constraints,
        |row| row.csdl_xml.clone(),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    );
    assert!(
        live_report.is_healthy(),
        "scratch-validated registry retry must activate identically"
    );
    live_registry.replace_tenant_restore_quarantine(tenant, None);
    live_report
}

async fn persist_failed_retry(
    registry: &std::sync::Arc<std::sync::RwLock<SpecRegistry>>,
    store: &dyn PlatformStore,
    tenant: &str,
    source: &RegistrySourceSnapshot,
    mut report: RegistryRestoreHealth,
) -> Result<RegistryRestoreHealth, RegistryRetryError> {
    let active = quarantine_upserts(&report);
    let replaced = store
        .replace_registry_restore_quarantines_for_tenant(tenant, source, &active)
        .await
        .map_err(|error| {
            RegistryRetryError::Storage(format!("Failed to persist retry quarantine: {error}"))
        })?;
    if !replaced {
        return Err(RegistryRetryError::Conflict(format!(
            "Committed source for tenant '{tenant}' changed while retry quarantine was persisted"
        )));
    }
    let durable = store
        .load_registry_restore_quarantines_for_tenant(tenant, REGISTRY_QUARANTINE_ENTITY_BUDGET + 1)
        .await
        .map_err(|error| {
            RegistryRetryError::Storage(format!("Failed to verify retry quarantine: {error}"))
        })?;
    if durable_snapshot(&durable) != report_snapshot(&report) {
        return Err(RegistryRetryError::Storage(format!(
            "Retry quarantine durability verification mismatch for tenant '{tenant}'"
        )));
    }
    apply_durable_acknowledgments(&mut report, &durable);
    registry
        .write()
        .unwrap_or_else(|error| error.into_inner())
        .replace_tenant_restore_quarantine(tenant, report.quarantined_tenants.get(tenant).cloned());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDER_IOA: &str = include_str!("../../../../../test-fixtures/specs/order.ioa.toml");
    const ORDER_CSDL: &str = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");

    fn spec_row() -> SpecRow {
        SpecRow {
            tenant: "tenant".to_string(),
            entity_type: "Order".to_string(),
            ioa_source: "[automaton]".to_string(),
            csdl_xml: Some("<Schema />".to_string()),
            content_hash: "hash".to_string(),
            version: 7,
            committed: true,
            verification_status: "pending".to_string(),
            verified: false,
            levels_passed: None,
            levels_total: None,
            verification_result: None,
            updated_at: "before".to_string(),
        }
    }

    #[test]
    fn retry_source_snapshot_ignores_verification_bookkeeping_only() {
        let original = spec_row();
        let mut verification_update = original.clone();
        verification_update.verification_status = "passed".to_string();
        verification_update.verified = true;
        verification_update.levels_passed = Some(3);
        verification_update.levels_total = Some(3);
        verification_update.verification_result = Some("{}".to_string());
        verification_update.updated_at = "after".to_string();
        assert_eq!(
            spec_source_snapshot(std::slice::from_ref(&original)),
            spec_source_snapshot(std::slice::from_ref(&verification_update))
        );

        verification_update.ioa_source.push_str("\nchanged = true");
        assert_ne!(
            spec_source_snapshot(std::slice::from_ref(&original)),
            spec_source_snapshot(std::slice::from_ref(&verification_update))
        );
    }

    #[test]
    fn activation_guard_preserves_a_newer_process_local_registry() {
        let tenant = "tenant";
        let tenant_id = TenantId::new(tenant);
        let csdl = parse_csdl(ORDER_CSDL).expect("parse test CSDL");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            tenant,
            csdl.clone(),
            ORDER_CSDL.to_string(),
            &[("Order", ORDER_IOA)],
        );
        let live_source_before = registry.tenant_source_snapshot(&tenant_id);
        let newer_ioa = format!("{ORDER_IOA}\n# same-process newer source");
        registry.register_tenant(
            tenant,
            csdl,
            ORDER_CSDL.to_string(),
            &[("Order", &newer_ioa)],
        );
        let registry = std::sync::Arc::new(std::sync::RwLock::new(registry));
        let candidate_rows = vec![SpecRow {
            tenant: tenant.to_string(),
            entity_type: "Order".to_string(),
            ioa_source: ORDER_IOA.to_string(),
            csdl_xml: Some(ORDER_CSDL.to_string()),
            content_hash: "older".to_string(),
            version: 1,
            committed: true,
            verification_status: "pending".to_string(),
            verified: false,
            levels_passed: None,
            levels_total: None,
            verification_result: None,
            updated_at: "before".to_string(),
        }];

        let report = activate_validated_retry(
            &registry,
            tenant,
            &tenant_id,
            live_source_before,
            candidate_rows,
            BTreeMap::new(),
            RegistryRestoreHealth {
                restored_specs: 1,
                quarantined_tenants: BTreeMap::new(),
            },
        );

        assert!(report.is_healthy());
        assert_eq!(
            registry
                .read()
                .expect("registry lock")
                .get_spec(&tenant_id, "Order")
                .expect("newer live Order")
                .ioa_source,
            newer_ioa,
            "repair must not regress a tenant registry that advanced while awaiting storage"
        );
    }
}
