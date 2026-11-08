//! Restoring a registry through the [`PlatformStore`] trait, the path the
//! simulation harness shares with production stores.

use std::collections::BTreeMap;

use super::converted::{ConvertedSpec, StoredSpec, read_stored_spec, write_back};
use super::restored_csdl_for_rows;
use crate::platform_store::{PlatformStore, SpecRow};
use crate::registry::SpecRegistry;

/// Restore a [`SpecRegistry`] from a [`PlatformStore`] (trait-based).
///
/// This is the production code path for restoring specs from any platform
/// store backend (Turso, Sim, etc.). Used by both the CLI bootstrap and
/// the DST harness — ensuring simulation runs identical code.
///
/// Unlike [`super::restore_registry_from_turso`], this does not restore verification
/// status or cross-entity constraints (the `PlatformStore` trait returns simpler
/// `SpecRow` types). Specs are registered with `Pending` verification status.
pub async fn restore_registry_from_platform_store(
    registry: &mut SpecRegistry,
    store: &dyn PlatformStore,
) -> Result<usize, String> {
    match store.delete_uncommitted_specs().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            "discarded {n} uncommitted spec versions during startup recovery; types with a committed version keep it"
        ),
        Err(e) => tracing::warn!("failed to discard uncommitted spec versions: {e}"),
    }
    let rows = store
        .load_specs()
        .await
        .map_err(|e| format!("Failed to read specs from platform store: {e}"))?;

    if rows.is_empty() {
        return Ok(0);
    }

    // Group by tenant.
    let mut grouped: BTreeMap<String, Vec<SpecRow>> = BTreeMap::new();
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

        // Specs stored by an earlier kernel in the old predicate syntax are
        // converted here, once, and written back. One that still cannot be
        // read disables only its own entity type and stays in the store.
        let mut readable: Vec<(String, String)> = Vec::with_capacity(tenant_rows.len());
        let mut converted: Vec<(&SpecRow, String)> = Vec::new();
        for row in tenant_rows {
            match read_stored_spec(tenant, &row.entity_type, &row.ioa_source) {
                StoredSpec::Current => {
                    readable.push((row.entity_type.clone(), row.ioa_source.clone()));
                }
                StoredSpec::Converted(source) => {
                    readable.push((row.entity_type.clone(), source.clone()));
                    converted.push((row, source));
                }
                StoredSpec::Unreadable => {}
            }
        }
        let ioa_pairs: Vec<(&str, &str)> = readable
            .iter()
            .map(|(entity_type, ioa)| (entity_type.as_str(), ioa.as_str()))
            .collect();

        match registry.try_register_tenant_with_constraints(
            tenant.as_str(),
            csdl,
            csdl_xml,
            &ioa_pairs,
            None,
            false,
        ) {
            Ok(()) => {
                restored_specs += readable.len();
                write_back_converted_rows(store, tenant, converted).await;
            }
            Err(e) => {
                tracing::warn!(tenant = %tenant, "reconciling orphaned specs for tenant that failed registration: {e}");
                for (entity_type, _) in &readable {
                    orphaned_specs.push((tenant.clone(), entity_type.clone()));
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

/// Write back a tenant's converted rows. These rows carry no verification
/// flag, so whether each was verified as stored comes from the store's
/// verification cache, matched on the row's own content hash.
async fn write_back_converted_rows(
    store: &dyn PlatformStore,
    tenant: &str,
    converted: Vec<(&SpecRow, String)>,
) {
    if converted.is_empty() {
        return;
    }
    let verified_before = store
        .load_verification_cache(tenant)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(tenant, %error, "failed to read verification before converting specs");
            BTreeMap::new()
        });
    let converted: Vec<ConvertedSpec> = converted
        .into_iter()
        .map(|(row, source)| ConvertedSpec {
            entity_type: row.entity_type.clone(),
            source,
            original: row.ioa_source.clone(),
            csdl_xml: row.csdl_xml.clone().unwrap_or_default(),
            verified: verified_before
                .get(&row.entity_type)
                .is_some_and(|(hash, verified)| *verified && *hash == row.content_hash),
        })
        .collect();
    write_back(store, tenant, &converted).await;
}
