//! Durable quarantine reconciliation and public restore operations.

use super::*;

mod retry;
mod snapshot;

pub use retry::{RegistryRetryError, retry_registry_tenant};
use snapshot::{
    apply_durable_acknowledgments, durable_snapshot, quarantine_upserts, report_snapshot,
};

async fn persist_restore_health(
    store: &dyn PlatformStore,
    source: &RegistrySourceSnapshot,
    report: &RegistryRestoreHealth,
) -> Result<Vec<RegistryQuarantineRecord>, String> {
    let active = quarantine_upserts(report);
    let replaced = store
        .replace_registry_restore_quarantines(source, &active)
        .await
        .map_err(|error| format!("Failed to persist registry restore quarantine: {error}"))?;
    if !replaced {
        return Err(
            "Failed to persist registry restore quarantine: committed source snapshot changed"
                .to_string(),
        );
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
    Ok(durable)
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
    let rows = store
        .load_specs()
        .await
        .map_err(|e| format!("Failed to read specs from platform store: {e}"))?;
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
    let mut outcome = populate_registry(
        registry,
        grouped,
        &mut constraints_by_tenant,
        |row| row.csdl_xml.clone(),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    );

    let durable = persist_restore_health(store, &source, &outcome).await?;
    apply_durable_acknowledgments(&mut outcome, &durable);
    registry.record_restore_health(&outcome);
    if !outcome.is_healthy() {
        tracing::warn!(
            quarantined_tenants = outcome.quarantined_tenants.len(),
            "quarantined specs with corrupt CSDL during platform-store restore; kept in store for inspection"
        );
    }

    Ok(outcome.restored_specs)
}
