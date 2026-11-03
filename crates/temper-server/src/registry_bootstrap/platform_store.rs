//! Restoring a registry through the [`PlatformStore`] trait, the path the
//! simulation harness shares with production stores.

use std::collections::BTreeMap;

use super::{StoredSpec, read_stored_spec, restored_csdl_for_rows};
use crate::registry::SpecRegistry;

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

        // Specs stored by an earlier kernel in the old predicate syntax are
        // converted here, once, and written back. One that still cannot be
        // read disables only its own entity type and stays in the store.
        let mut readable: Vec<(String, String)> = Vec::with_capacity(tenant_rows.len());
        let mut converted: Vec<(&crate::platform_store::SpecRow, String)> = Vec::new();
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
                persist_converted_specs(store, tenant, &converted).await;
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

/// Write converted specs back so the conversion happens once. A failed write
/// only means the next boot converts the stored source again.
///
/// Writing a new source resets the row's verification, and nothing re-verifies
/// a restored spec, so its entity type would refuse writes (423). The
/// conversion keeps the spec's meaning, so a spec that was verified as stored
/// stays verified as converted, as the kernel already trusts prebuilt bundles.
async fn persist_converted_specs(
    store: &dyn crate::platform_store::PlatformStore,
    tenant: &str,
    converted: &[(&crate::platform_store::SpecRow, String)],
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
    let mut carry_verification = Vec::new();
    let mut written: Vec<(&str, String, &str)> = Vec::new();
    for (row, source) in converted {
        let hash = temper_store_turso::spec_content_hash(source);
        let csdl_xml = row.csdl_xml.as_deref().unwrap_or_default();
        if let Err(error) = store
            .upsert_spec(tenant, &row.entity_type, source, csdl_xml, &hash)
            .await
        {
            tracing::warn!(tenant, entity_type = %row.entity_type, %error, "failed to store converted spec");
            continue;
        }
        written.push((row.entity_type.as_str(), hash, csdl_xml));
        if verified_before
            .get(&row.entity_type)
            .is_some_and(|(cached_hash, verified)| *verified && *cached_hash == row.content_hash)
        {
            carry_verification.push(row.entity_type.as_str());
        }
    }
    for entity_type in carry_verification {
        if let Err(error) = store
            .persist_spec_verification(
                tenant,
                entity_type,
                crate::platform_store::SpecVerificationUpdate {
                    status: "completed",
                    verified: true,
                    levels_passed: None,
                    levels_total: None,
                    verification_result_json: None,
                },
            )
            .await
        {
            tracing::warn!(tenant, entity_type, %error, "failed to carry verification to converted spec");
        }
    }
    // upsert_spec marks a changed row uncommitted; commit exactly the
    // converted rows, or restore keeps loading the stored version.
    let versions: Vec<(&str, &str, &str)> = written
        .iter()
        .map(|(entity_type, hash, csdl_xml)| (*entity_type, hash.as_str(), *csdl_xml))
        .collect();
    match store.commit_spec_versions(tenant, &versions, &[]).await {
        Ok(true) => {}
        Ok(false) => tracing::warn!(tenant, "converted specs changed before commit"),
        Err(error) => tracing::warn!(tenant, %error, "failed to commit converted specs"),
    }
}
