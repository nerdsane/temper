//! Restoring the tenants left for after the server serves, one at a time
//! (see [`temper_server::tenant_restore`] for what their requests see).
//!
//! A tenant's restore does for it what the boot did for the server's own
//! tenants, in the same order: its specs, its Cedar policies, secrets and
//! WASM modules, its agent specs when it has a store of its own, its apps,
//! its route table, then its timers. A read of its store that fails ends the
//! attempt: the tenant stays starting and is tried again, from the step that
//! failed.

use std::collections::BTreeMap;

use temper_platform::genesis_install;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::authz::try_load_and_activate_tenant_policies;
use temper_server::registry_bootstrap::{
    StoredTenant, read_stored_tenants_from_turso, register_stored_tenant,
};

use super::bootstrap::{bootstrap_connected_tenant, hydrate_tenants};
use super::installed_apps::replay_recorded_installs;
use super::later::{Later, LaterTenant};

/// Restore each tenant `later` holds, in the background, the one a request
/// waits for first.
pub(super) fn spawn_walk(state: PlatformState, mut later: Later) {
    let queue = state.server.tenant_restore.clone();
    queue.begin(later.tenants.keys().map(TenantId::new));
    // determinism-ok: the restore walk runs in the background, after the server serves.
    tokio::spawn(async move {
        while let Some(tenant) = queue.next().await {
            let left = later
                .tenants
                .remove(tenant.as_str())
                .expect("a starting tenant is one left for later");
            let attempt = tokio::spawn(attempt(state.clone(), tenant.clone(), left)); // determinism-ok: one restore at a time
            let outcome = match attempt.await {
                Ok((outcome, left)) => {
                    if outcome.is_err() {
                        later.tenants.insert(tenant.to_string(), left);
                    }
                    outcome
                }
                // What it needed went with it, so it cannot be tried again:
                // its requests are told it is unavailable, never served.
                Err(error) => {
                    let reason = format!("its restore stopped: {error}");
                    tracing::error!(tenant = %tenant, %reason, "tenant left unavailable");
                    state
                        .registry
                        .write()
                        .expect("registry lock poisoned")
                        .mark_tenant_unavailable(tenant.clone(), reason);
                    Ok(())
                }
            };
            if let Err(reason) = &outcome {
                tracing::warn!(tenant = %tenant, %reason, "tenant restore failed; it stays starting and is tried again");
            }
            queue.ended(&tenant, outcome);
        }
        tracing::info!("every tenant left for after serving is restored");
    });
}

/// One attempt at restoring `tenant`, handing back what is left to do.
async fn attempt(
    state: PlatformState,
    tenant: TenantId,
    mut left: LaterTenant,
) -> (Result<(), String>, LaterTenant) {
    let hold = state.server.hold_timer_follow(&tenant);
    let outcome = restore(&state, &tenant, &mut left).await;
    // The restore walks every timed type of the tenant itself, or will on
    // its next attempt.
    state
        .registry
        .write()
        .expect("registry lock poisoned")
        .discard_changed_types(&tenant);
    drop(hold);
    (outcome, left)
}

/// Restore `tenant` from what `left` holds, taking each step's part as it
/// is done, so a later attempt goes on from the step that failed.
async fn restore(
    state: &PlatformState,
    tenant: &TenantId,
    left: &mut LaterTenant,
) -> Result<(), String> {
    let name = tenant.as_str();
    if left.routed && left.specs.is_none() && !left.registered {
        left.specs = read_routed_specs(state, name).await?;
    }
    if !left.registered {
        if let Some(specs) = left.specs.take() {
            let mut registry = state.registry.write().expect("registry lock poisoned");
            register_stored_tenant(&mut registry, name.to_string(), specs);
        }
        left.registered = true;
    }
    // Its stored rows do not load: its requests are told so.
    if state.tenant_unavailable_reason(name).is_some() {
        return Ok(());
    }
    state.server.rebuild_reaction_dispatcher();

    if let Some(text) = left.legacy_policies.take() {
        match state.server.authz.reload_tenant_policies(name, &text) {
            Ok(()) => {
                state
                    .server
                    .tenant_policies
                    .write()
                    .expect("tenant policies lock poisoned")
                    .insert(name.to_string(), text);
            }
            Err(e) => tracing::warn!(tenant = name, error = %e, "skipping invalid Cedar policies"),
        }
    }
    try_load_and_activate_tenant_policies(&state.server, name)
        .await
        .map_err(|e| format!("its Cedar policies could not be read: {e}"))?;
    state
        .server
        .load_tenant_secrets(name)
        .await
        .map_err(|e| format!("its secrets could not be read: {e}"))?;
    state
        .server
        .load_tenant_wasm_modules(name)
        .await
        .map_err(|e| format!("its WASM modules could not be read: {e}"))?;
    if std::mem::take(&mut left.routed) {
        bootstrap_connected_tenant(state, name).await;
        hydrate_tenants(state, vec![tenant.clone()]).await;
    }

    let roots = std::mem::take(&mut left.cache_roots);
    genesis_install::restore_planned_cache_roots(state, roots).await;
    let installs = std::mem::take(&mut left.installs);
    let recorded = installs.iter().map(|app| (name.to_string(), app.clone()));
    genesis_install::start_registry_cache_recovery_of(state, recorded.collect()).await;
    replay_recorded_installs(state, name, installs).await;

    temper_server::http_endpoint::rebuild_tenant_table(&state.server, tenant).await;
    state.server.start_tenant_runtime(tenant).await
}

/// The spec rows of `tenant`'s own store (a routed store).
async fn read_routed_specs(
    state: &PlatformState,
    tenant: &str,
) -> Result<Option<StoredTenant>, String> {
    let Some(provider) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.turso.clone())
    else {
        return Ok(None);
    };
    let Some(store) = provider.store_for_tenant(tenant).await else {
        return Ok(None);
    };
    let mut stored: BTreeMap<String, StoredTenant> =
        read_stored_tenants_from_turso(&store)
            .await
            .map_err(|e| format!("its specs could not be read: {e}"))?;
    Ok(stored.remove(tenant))
}
