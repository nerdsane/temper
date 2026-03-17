//! Production recovery functions for platform state on restart.
//!
//! These functions are the **single source of truth** for restoring in-memory
//! platform state from durable storage after a restart. Both the CLI bootstrap
//! pipeline and the DST harness call these identical functions — no test-only
//! reimplementations.
//!
//! Follows the FoundationDB DST principle: swap the I/O, keep the code.

use temper_runtime::tenant::TenantId;
use temper_server::platform_store::PlatformStore;

use crate::os_apps;
use crate::state::PlatformState;

/// Recover Cedar policies from the platform store into memory.
///
/// Loads all tenant policies from the durable store, validates each one
/// individually (so one bad tenant doesn't block others), inserts them
/// into the in-memory `tenant_policies` map, and rebuilds the authorization
/// engine with all policies combined.
///
/// This is the **production code path** — identical logic runs at CLI boot
/// and during DST restart simulation.
pub async fn recover_cedar_policies(state: &PlatformState, ps: &dyn PlatformStore) {
    let all_policy_rows = match ps.load_tenant_policies().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("Failed to load Cedar policies from platform store: {e}");
            return;
        }
    };

    if all_policy_rows.is_empty() {
        return;
    }

    let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
    let mut loaded_count = 0usize;
    for (tenant, policy_text) in &all_policy_rows {
        // Validate each tenant's policies individually so one bad tenant
        // doesn't prevent all others from loading.
        if temper_authz::AuthzEngine::new(policy_text).is_err() {
            tracing::warn!("Skipping invalid Cedar policies for tenant '{tenant}'");
            continue;
        }
        policies.insert(tenant.clone(), policy_text.clone());
        loaded_count += 1;
    }
    let mut combined = String::new();
    for text in policies.values() {
        combined.push_str(text);
        combined.push('\n');
    }
    if let Err(e) = state.server.authz.reload_policies(&combined) {
        tracing::warn!("Failed to reload Cedar policies: {e}");
    } else if loaded_count > 0 {
        tracing::info!("Restored Cedar policies for {loaded_count} tenants.");
    }
}

/// Restore previously installed OS apps from the platform store.
///
/// Reads the durable `tenant_installed_apps` table and reinstalls any
/// OS apps whose specs are not already present in the SpecRegistry.
/// Uses the production [`os_apps::install_os_app`] code path — no shortcuts.
///
/// This is the **production code path** — identical logic runs at CLI boot
/// (Phase 8b) and during DST restart simulation.
pub async fn restore_installed_os_apps(state: &PlatformState, ps: &dyn PlatformStore) {
    let installed = match ps.list_all_installed_apps().await {
        Ok(apps) => apps,
        Err(e) => {
            tracing::warn!("Failed to load installed OS apps: {e}");
            return;
        }
    };

    for (tenant, app_name) in installed {
        // Check if the app's entity types are already in the registry.
        if tenant_has_os_app_specs(state, &tenant, &app_name) {
            continue;
        }

        match os_apps::install_os_app(state, &tenant, &app_name).await {
            Ok(result) => {
                let all: Vec<String> = result
                    .added
                    .iter()
                    .chain(&result.updated)
                    .chain(&result.skipped)
                    .cloned()
                    .collect();
                tracing::info!(
                    "Restored OS app '{app_name}' for '{tenant}': {}",
                    all.join(", ")
                );
            }
            Err(e) => {
                tracing::warn!("Failed to restore OS app '{app_name}' for '{tenant}': {e}");
            }
        }
    }
}

/// Check if all entity types for an OS app are already registered.
fn tenant_has_os_app_specs(state: &PlatformState, tenant: &str, app_name: &str) -> bool {
    let Some(bundle) = os_apps::get_os_app(app_name) else {
        return false;
    };
    let tenant_id = TenantId::new(tenant);
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    bundle
        .specs
        .iter()
        .all(|(entity_type, _)| registry.get_table(&tenant_id, entity_type).is_some())
}
