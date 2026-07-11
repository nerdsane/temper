//! Durable quarantine reconciliation and public restore operations.

use super::*;

mod retry;
mod snapshot;

pub use retry::{RegistryRetryError, retry_registry_tenant};
use snapshot::{
    apply_durable_acknowledgments, durable_snapshot, quarantine_upserts, report_snapshot,
};

const REGISTRY_RESTORE_CAS_ATTEMPT_BUDGET: usize = 3;

struct PreparedRestore {
    registry: SpecRegistry,
    source: RegistrySourceSnapshot,
    outcome: RegistryRestoreHealth,
}

async fn persist_restore_health(
    store: &dyn PlatformStore,
    source: &RegistrySourceSnapshot,
    report: &RegistryRestoreHealth,
) -> Result<Option<Vec<RegistryQuarantineRecord>>, String> {
    let active = quarantine_upserts(report);
    let replaced = store
        .replace_registry_restore_quarantines(source, &active)
        .await
        .map_err(|error| format!("Failed to persist registry restore quarantine: {error}"))?;
    if !replaced {
        return Ok(None);
    }
    let durable = store
        .load_registry_restore_quarantines()
        .await
        .map_err(|error| format!("Failed to verify registry restore quarantine: {error}"))?;
    let expected = report_snapshot(report);
    let actual = durable_snapshot(&durable);
    if actual != expected {
        return Err(format!(
            "Registry quarantine durability verification mismatch: expected {} active records, loaded {}",
            expected.len(),
            actual.len()
        ));
    }
    Ok(Some(durable))
}

async fn prepare_restore(store: &dyn PlatformStore) -> Result<PreparedRestore, String> {
    let rows = store
        .load_specs()
        .await
        .map_err(|error| format!("Failed to read specs from platform store: {error}"))?;
    let constraint_rows = store
        .load_tenant_constraints()
        .await
        .map_err(|error| format!("Failed to read tenant constraints: {error}"))?;
    let source = RegistrySourceSnapshot::from_rows(&rows, &constraint_rows)?;
    let mut constraints_by_tenant = constraint_rows
        .into_iter()
        .map(|row| (row.tenant, (row.source, row.version)))
        .collect::<BTreeMap<_, _>>();
    let mut grouped: BTreeMap<String, Vec<SpecRow>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.tenant.clone()).or_default().push(row);
    }

    // Compilation happens in a detached registry. A stale manifest can never
    // leak its activation into the live registry before the durable CAS wins.
    let mut registry = SpecRegistry::new();
    let outcome = populate_registry(
        &mut registry,
        grouped,
        &mut constraints_by_tenant,
        |row| row.csdl_xml.clone(),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    );
    Ok(PreparedRestore {
        registry,
        source,
        outcome,
    })
}

fn activate_prepared_restore(
    target: &mut SpecRegistry,
    prepared: &SpecRegistry,
) -> Result<(), String> {
    let tenants = prepared
        .tenant_ids()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    for tenant in tenants {
        let config = prepared
            .get_tenant(&tenant)
            .ok_or_else(|| format!("prepared registry lost tenant '{tenant}'"))?;
        let ioa_sources = config
            .entities
            .iter()
            .map(|(entity_type, spec)| (entity_type.clone(), spec.ioa_source.clone()))
            .collect::<Vec<_>>();
        let ioa_pairs = ioa_sources
            .iter()
            .map(|(entity_type, source)| (entity_type.as_str(), source.as_str()))
            .collect::<Vec<_>>();
        target
            .try_register_tenant_with_reactions_and_constraints(
                tenant.clone(),
                config.csdl.as_ref().clone(),
                config.csdl_xml.as_ref().clone(),
                &ioa_pairs,
                config.reactions.clone(),
                config.cross_invariants_source.clone(),
                false,
            )
            .map_err(|error| {
                format!("Failed to activate validated registry tenant '{tenant}': {error}")
            })?;
        for (entity_type, status) in &config.verification {
            target.set_verification_status(&tenant, entity_type, status.clone());
        }
    }
    Ok(())
}

/// Restore through the real Postgres platform adapter.
pub async fn restore_registry_from_postgres(
    registry: &mut SpecRegistry,
    store: &PostgresEventStore,
) -> Result<usize, String> {
    restore_registry_from_platform_store(registry, store).await
}

/// Restore through the real Turso platform adapter.
pub async fn restore_registry_from_turso(
    registry: &mut SpecRegistry,
    store: &TursoEventStore,
) -> Result<usize, String> {
    restore_registry_from_platform_store(registry, store).await
}

/// Restore a [`SpecRegistry`] from a [`PlatformStore`] (trait-based).
///
/// This is the canonical production and DST path. Concrete Postgres, Turso, and
/// simulation adapters differ only in storage I/O; grouping, verification-state
/// recovery, fault isolation, durable quarantine, and health semantics are shared.
pub async fn restore_registry_from_platform_store(
    registry: &mut SpecRegistry,
    store: &dyn PlatformStore,
) -> Result<usize, String> {
    match store.delete_uncommitted_specs().await {
        Ok(0) => {}
        Ok(n) => tracing::info!("deleted {n} uncommitted specs during startup recovery"),
        Err(e) => tracing::warn!("failed to delete uncommitted specs: {e}"),
    }
    for attempt in 1..=REGISTRY_RESTORE_CAS_ATTEMPT_BUDGET {
        let mut prepared = prepare_restore(store).await?;
        let Some(durable) =
            persist_restore_health(store, &prepared.source, &prepared.outcome).await?
        else {
            if attempt < REGISTRY_RESTORE_CAS_ATTEMPT_BUDGET {
                tracing::warn!(
                    attempt,
                    attempt_budget = REGISTRY_RESTORE_CAS_ATTEMPT_BUDGET,
                    "registry restore source changed before quarantine reconciliation; reloading"
                );
                continue;
            }
            return Err(format!(
                "Failed to persist registry restore quarantine: committed source snapshot changed on all {REGISTRY_RESTORE_CAS_ATTEMPT_BUDGET} attempts"
            ));
        };

        apply_durable_acknowledgments(&mut prepared.outcome, &durable);
        activate_prepared_restore(registry, &prepared.registry)?;
        registry.record_restore_health(&prepared.outcome);
        if !prepared.outcome.is_healthy() {
            tracing::warn!(
                quarantined_tenants = prepared.outcome.quarantined_tenants.len(),
                "quarantined specs with corrupt CSDL during platform-store restore; kept in store for inspection"
            );
        }
        return Ok(prepared.outcome.restored_specs);
    }
    unreachable!("positive restore attempt budget must return from the loop")
}
