//! Production recovery functions for platform state on restart.
//!
//! These functions are the **single source of truth** for restoring in-memory
//! platform state from durable storage after a restart. Both the CLI bootstrap
//! pipeline and the DST harness call these identical functions — no test-only
//! reimplementations.
//!
//! Follows the FoundationDB DST principle: swap the I/O, keep the code.

use std::collections::{BTreeMap, BTreeSet};

use temper_server::platform_store::PlatformStore;

use crate::os_apps;
use crate::state::PlatformState;

/// Runtime-only outcome for installed-app warm restart recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledAppRuntimeRecoveryOutcome {
    /// Specs were already registered and marked usable.
    Ready,
    /// Specs were registered and the durable bundle digest matched; runtime
    /// verification readiness was repaired without reinstalling app content.
    Healed,
    /// The app should be reconciled by the digest-aware app reconcile path.
    NeedsReconcile,
    /// The durable installed-app row points at an app that is not in the catalog.
    MissingBundle,
    /// Durable metadata could not be read, so the caller should reconcile.
    StoreError,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstalledAppsRuntimeRecoverySummary {
    pub ready: usize,
    pub healed: usize,
    pub needs_reconcile: usize,
    pub missing_bundle: usize,
    pub store_error: usize,
}

/// Recover Cedar policies from the platform store into memory.
///
/// Loads legacy tenant policy blobs and granular per-policy rows from durable
/// storage, validates each tenant independently, and activates them in the
/// per-tenant Cedar engine.
///
/// This is the **production code path** — identical logic runs at CLI boot
/// and during DST restart simulation.
pub async fn recover_cedar_policies(state: &PlatformState, ps: &dyn PlatformStore) {
    let mut legacy_entries: BTreeMap<String, String> = BTreeMap::new();
    let mut granular_entries: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut granular_tenants = BTreeSet::new();

    match ps.load_tenant_policies().await {
        Ok(rows) => {
            for (tenant, policy_text) in rows {
                if policy_text.trim().is_empty() {
                    continue;
                }
                legacy_entries.insert(tenant, policy_text);
            }
        }
        Err(e) => {
            tracing::warn!("Failed to load legacy Cedar policies from platform store: {e}");
        }
    }

    match ps.load_policy_entries().await {
        Ok(rows) => {
            for row in rows {
                granular_tenants.insert(row.tenant.clone());
                if !row.enabled || row.cedar_text.trim().is_empty() {
                    continue;
                }
                granular_entries
                    .entry(row.tenant)
                    .or_default()
                    .push((row.policy_id, row.cedar_text));
            }
        }
        Err(e) => {
            tracing::warn!("Failed to load granular Cedar policies from platform store: {e}");
        }
    }

    if legacy_entries.is_empty() && granular_tenants.is_empty() {
        return;
    }

    let tenants: BTreeSet<String> = legacy_entries
        .keys()
        .chain(granular_tenants.iter())
        .cloned()
        .collect();
    let mut loaded_count = 0usize;
    let mut loaded_policy_count = 0usize;
    let mut skipped_legacy_count = 0usize;
    for tenant in tenants {
        let entries = granular_entries.remove(&tenant).unwrap_or_default();
        let mut policy_text = String::new();
        let mut entry_count = 0usize;

        // Row presence establishes the canonical granular generation even when
        // every row is disabled. The aggregate is only a compatibility cache and
        // must not revive removed or unowned grants during restart.
        if granular_tenants.contains(&tenant) {
            if legacy_entries.contains_key(&tenant) {
                skipped_legacy_count += 1;
            }
        } else if let Some(legacy_text) = legacy_entries.get(&tenant) {
            append_cedar_policy_text(&mut policy_text, legacy_text);
            entry_count += 1;
        }

        for (_, cedar_text) in entries {
            append_cedar_policy_text(&mut policy_text, &cedar_text);
            entry_count += 1;
        }

        // Use the raw policy reload path here instead of per-row PolicyId
        // rewriting. Production primary policies can contain tens of thousands
        // of generated statements; raw reload matches the pre-existing startup
        // path and avoids deep Cedar policy cloning during restart recovery.
        if let Err(e) = state
            .server
            .authz
            .reload_tenant_policies(&tenant, &policy_text)
        {
            tracing::warn!(tenant, error = %e, "Skipping invalid Cedar policies for tenant");
            continue;
        }

        if let Some(policy_text) = state.server.authz.get_tenant_policy_text(&tenant)
            && let Ok(mut policies) = state.server.tenant_policies.write()
        {
            policies.insert(tenant.clone(), policy_text);
        }

        loaded_count += 1;
        loaded_policy_count += entry_count;
    }

    if loaded_count > 0 {
        tracing::info!(
            tenants = loaded_count,
            policies = loaded_policy_count,
            skipped_legacy = skipped_legacy_count,
            "Restored Cedar policies from durable storage."
        );
    }
}

fn append_cedar_policy_text(target: &mut String, cedar_text: &str) {
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(cedar_text);
}

/// Restore previously installed OS apps from the platform store.
///
/// Reads the durable `tenant_installed_apps` table and reinstalls any
/// apps whose specs are not already present in the SpecRegistry.
/// Uses the production [`os_apps::install_os_app`] code path — no shortcuts.
///
/// This is the **production code path** — identical logic runs at CLI boot
/// (Phase 8b) and during DST restart simulation.
pub async fn restore_installed_apps(state: &PlatformState, ps: &dyn PlatformStore) {
    let installed = match ps.list_all_installed_apps().await {
        Ok(apps) => apps,
        Err(e) => {
            tracing::warn!("Failed to load installed os-apps: {e}");
            return;
        }
    };

    for (tenant, app_name) in installed {
        match recover_installed_app_runtime_state(state, ps, &tenant, &app_name).await {
            InstalledAppRuntimeRecoveryOutcome::Ready => {
                continue;
            }
            InstalledAppRuntimeRecoveryOutcome::Healed => {
                tracing::info!(
                    tenant,
                    app = %app_name,
                    "Restored installed app runtime readiness without hot content bootstrap"
                );
                continue;
            }
            InstalledAppRuntimeRecoveryOutcome::NeedsReconcile
            | InstalledAppRuntimeRecoveryOutcome::MissingBundle
            | InstalledAppRuntimeRecoveryOutcome::StoreError => {}
        }

        // Runtime readiness alone cannot prove content, seed, or terminal
        // metadata completion. The digest-aware reconciliation plan deliberately
        // retries every phase for a non-installed record.
        match os_apps::reconcile_os_app(state, &tenant, &app_name).await {
            Ok(result) => {
                tracing::info!(
                    "Reconciled app '{app_name}' for '{tenant}' during recovery: {result:?}"
                );
            }
            Err(e) => {
                tracing::warn!("Failed to restore app '{app_name}' for '{tenant}': {e}");
            }
        }
    }
}

/// Recover warm-restart runtime state for one installed app without running the
/// full OS-app install/bootstrap path.
///
/// This is intentionally bounded: it never writes APP.md, skills, agents, ADRs,
/// system files, or seed entities. If durable metadata cannot prove the bundle
/// is unchanged, callers get [`InstalledAppRuntimeRecoveryOutcome::NeedsReconcile`]
/// and should use digest-aware reconcile for the required app surface.
pub async fn recover_installed_app_runtime_state(
    state: &PlatformState,
    ps: &dyn PlatformStore,
    tenant: &str,
    app_name: &str,
) -> InstalledAppRuntimeRecoveryOutcome {
    let Some(bundle) = os_apps::get_os_app(app_name) else {
        tracing::warn!(
            tenant,
            app = %app_name,
            "Installed OS app is missing from catalog; runtime recovery cannot restore it"
        );
        return InstalledAppRuntimeRecoveryOutcome::MissingBundle;
    };

    let Some(digest) = os_apps::os_app_bundle_digest(app_name) else {
        return InstalledAppRuntimeRecoveryOutcome::MissingBundle;
    };

    let record = match ps.get_installed_app(tenant, app_name).await {
        Ok(Some(record)) => record,
        Ok(None) => return InstalledAppRuntimeRecoveryOutcome::NeedsReconcile,
        Err(error) => {
            tracing::warn!(
                tenant,
                app = %app_name,
                error = %error,
                "Failed to read installed OS app metadata during runtime recovery"
            );
            return InstalledAppRuntimeRecoveryOutcome::StoreError;
        }
    };
    // Runtime artifacts are not proof that APP.md, skills, seed rows, or final
    // metadata committed. Only the terminal install status may use the warm
    // readiness fast path.
    if record.status != "installed" || record.bundle_digest != digest.bundle_digest {
        return InstalledAppRuntimeRecoveryOutcome::NeedsReconcile;
    }

    let specs_ready = os_apps::tenant_has_ready_app_specs_for_bundle(state, tenant, &bundle);
    let policies_active = os_apps::tenant_has_active_policies_for_bundle(state, tenant, &bundle);
    let wasm_registered = os_apps::tenant_has_registered_wasm_for_bundle(state, tenant, &bundle);

    if specs_ready && policies_active && wasm_registered {
        return InstalledAppRuntimeRecoveryOutcome::Ready;
    }

    let specs_ready = specs_ready
        || os_apps::restore_app_specs_from_matching_digest(state, ps, tenant, app_name, &bundle)
            .await;
    if specs_ready && policies_active && wasm_registered {
        InstalledAppRuntimeRecoveryOutcome::Healed
    } else {
        InstalledAppRuntimeRecoveryOutcome::NeedsReconcile
    }
}

/// Recover runtime readiness for all durable installed apps without reinstalling
/// app content.
pub async fn recover_installed_apps_runtime_state(
    state: &PlatformState,
    ps: &dyn PlatformStore,
) -> InstalledAppsRuntimeRecoverySummary {
    let installed = match ps.list_all_installed_apps().await {
        Ok(apps) => apps,
        Err(e) => {
            tracing::warn!("Failed to load installed os-apps for runtime recovery: {e}");
            return InstalledAppsRuntimeRecoverySummary {
                store_error: 1,
                ..InstalledAppsRuntimeRecoverySummary::default()
            };
        }
    };

    let mut summary = InstalledAppsRuntimeRecoverySummary::default();
    for (tenant, app_name) in installed {
        match recover_installed_app_runtime_state(state, ps, &tenant, &app_name).await {
            InstalledAppRuntimeRecoveryOutcome::Ready => summary.ready += 1,
            InstalledAppRuntimeRecoveryOutcome::Healed => summary.healed += 1,
            InstalledAppRuntimeRecoveryOutcome::NeedsReconcile => summary.needs_reconcile += 1,
            InstalledAppRuntimeRecoveryOutcome::MissingBundle => summary.missing_bundle += 1,
            InstalledAppRuntimeRecoveryOutcome::StoreError => summary.store_error += 1,
        }
    }
    summary
}

#[cfg(test)]
mod tests;
