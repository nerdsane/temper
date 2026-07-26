//! Startup entity hydration and durable key-contract activation.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;

fn registered_activation_tenants(
    all_tenants: &[TenantId],
    _registered_tenants: &BTreeSet<TenantId>,
) -> Vec<TenantId> {
    all_tenants.to_vec()
}

pub(in crate::serve) async fn hydrate_entities(
    state: &PlatformState,
    apps: &[(String, String)],
) -> Result<()> {
    if state.server.storage_stack.is_none() {
        return Ok(());
    }
    let eager_hydrate = std::env::var("TEMPER_EAGER_HYDRATE") // determinism-ok: read once at startup
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        })
        .unwrap_or(false);
    let mut all_tenants = Vec::new();
    for (tenant, _dir) in apps {
        all_tenants.push(TenantId::new(tenant.as_str()));
    }
    // In TenantRouted mode, also hydrate all registered tenants.
    if let Some(provider) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.turso.clone())
    {
        for tenant in provider.connected_tenants().await {
            all_tenants.push(TenantId::new(&tenant));
        }
    }
    // Postgres-restored tenants may have no CLI app directory and no Turso
    // router entry. The durable registry is the complete startup authority.
    let registered_tenants = state
        .server
        .registry
        .read()
        .expect("spec registry lock poisoned")
        .tenant_ids()
        .into_iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    all_tenants.extend(registered_tenants.iter().cloned());
    if let Some(storage) = state.server.storage_stack.as_ref() {
        let activated_contracts = storage
            .events
            .key_index_activated_contracts()
            .await
            .map_err(anyhow::Error::msg)
            .context("failed to enumerate activated key-contract tenants before serving")?;
        all_tenants.extend(
            activated_contracts
                .into_iter()
                .map(|(tenant, _)| TenantId::new(&tenant)),
        );
    }

    all_tenants.sort();
    all_tenants.dedup();
    // Establish each durable table's activation epoch before hydration can
    // dispatch actors or any network listener can serve writes.
    for tenant_id in registered_activation_tenants(&all_tenants, &registered_tenants) {
        state
            .server
            .activate_registered_key_contracts(&tenant_id)
            .await
            .map_err(anyhow::Error::msg)
            .with_context(|| {
                format!(
                    "failed to activate registered key contracts for tenant {tenant_id} before serving"
                )
            })?;
    }
    for tenant_id in &all_tenants {
        if eager_hydrate {
            state.server.hydrate_from_store(tenant_id).await;
        } else {
            state.server.populate_index_from_store(tenant_id).await;
        }
    }

    // Background task: backfill the declared-key index, then the broad field index,
    // from snapshots — after the entity index is populated so pre-existing entities
    // are covered.
    let server = state.server.clone();
    tokio::spawn(async move {
        // ADR-0153: the cheap declared-key backfill first (K = 1-3 rows per entity).
        // It keys pre-existing entities and sets the per-(tenant,type) watermark, so
        // their point reads resolve present/absent in O(log n) instead of the
        // full-type scan that 413s at tenant scale — independent of the heavy
        // field-index re-scan. No-op on backends that don't co-commit keys (those
        // never become authoritative, so a keyed miss stays scan-safe).
        for tenant_id in &all_tenants {
            server.populate_key_index_from_snapshots(tenant_id).await;
        }
        // ADR-0155: backfill the declared-vector index (parse + upsert one row per
        // declared path per entity), so pre-existing / write-behind entities are
        // rankable by Temper.Nearest and the per-type watermark is set.
        for tenant_id in &all_tenants {
            server.populate_vector_index_from_snapshots(tenant_id).await;
        }
        // Then the broad field index for OData filter push-down.
        for tenant_id in all_tenants {
            server.populate_field_index_from_snapshots(&tenant_id).await;
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use temper_runtime::tenant::TenantId;

    use super::registered_activation_tenants;

    #[test]
    fn durable_contract_only_tenant_is_not_activated_without_registered_specs() {
        let registered = TenantId::new("registered");
        let durable_only = TenantId::new("durable-only");
        let all_tenants = vec![durable_only, registered.clone()];
        let registered_tenants = BTreeSet::from([registered.clone()]);

        assert_eq!(
            registered_activation_tenants(&all_tenants, &registered_tenants),
            vec![registered]
        );
    }
}
