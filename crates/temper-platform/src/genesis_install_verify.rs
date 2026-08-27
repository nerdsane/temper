//! Genesis install verification and the pure verify/rollback decision.
//!
//! Extracted from `genesis_install.rs` to keep that file within the readability budget and to give
//! the verify+rollback contract (ARN-421) a cohesive home. The stateful rollback effects that need
//! the private materialize/reconcile helpers stay in `genesis_install.rs`; this module holds the
//! pieces that depend only on the catalog, the recovery probe, and the wasm engine.

use std::collections::BTreeMap;

use temper_server::platform_store::InstalledAppRecord;

use crate::genesis_install::reconcile_materialized_app_closure;
use crate::os_apps::{WasmModuleManifest, get_os_app, os_app_bundle_digest};
use crate::recovery::{InstalledAppRuntimeRecoveryOutcome, recover_installed_app_runtime_state};
use crate::state::PlatformState;

/// What to do after reconciling a new Genesis app version and checking its runtime readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallVerifyDecision {
    /// The new version verified runtime-ready; keep it and record its provenance.
    Commit,
    /// The new version failed verification and a prior good Genesis install exists; restore it.
    RollBackToPrevious,
    /// The new version failed verification and there is no prior install to restore; fail.
    FailNoRollback,
}

/// Decide the install outcome from the new version's verification result and the prior record.
///
/// Pure and side-effect free so it is unit- and simulation-testable. `new_version_ready` is the
/// verification verdict (runtime-ready AND every app-required wasm module compiles). A rollback
/// target must itself be a Genesis install that was in the `installed` status — anything else
/// (absent, a non-Genesis local install, or a prior failed install) is not a safe last-good state
/// to revert to, so the install fails cleanly instead.
pub(crate) fn classify_install_verify(
    new_version_ready: bool,
    prior_record: Option<&InstalledAppRecord>,
) -> InstallVerifyDecision {
    if new_version_ready {
        InstallVerifyDecision::Commit
    } else if prior_record
        .is_some_and(|record| record.source_kind == "genesis" && record.status == "installed")
    {
        InstallVerifyDecision::RollBackToPrevious
    } else {
        InstallVerifyDecision::FailNoRollback
    }
}

/// Verify every app in a freshly-reconciled Genesis closure is runtime-ready.
///
/// A Genesis install materializes and reconciles a whole closure — the root app plus its
/// dependency apps — so verification must cover every ref, not just the root. A dependency whose
/// required wasm does not compile is the same "failed to compile lazy-loaded WASM module" break,
/// one level down. Returns true only when all refs verify.
pub(crate) async fn verify_install_closure_runtime_ready(
    platform: &PlatformState,
    ps: &dyn temper_server::platform_store::PlatformStore,
    tenant: &str,
    app_names: &[String],
) -> bool {
    for app_name in app_names {
        if !verify_install_runtime_ready(platform, ps, tenant, app_name).await {
            return false;
        }
    }
    true
}

/// Verify a freshly-reconciled Genesis app is runtime-ready and every app-required wasm module
/// compiles.
///
/// Two checks, both required:
/// 1. `recover_installed_app_runtime_state` reports `Ready` or `Healed` — specs registered,
///    policies active, required wasm registered.
/// 2. Every app-required wasm module in the bundle actually compiles (see
///    [`required_wasm_modules_compile`]). Reconcile only eager-compiles modules flagged
///    `startup_loading = eager` and merely warns on failure, so a required module that is
///    lazy-loaded (or eager-but-broken) would otherwise install cleanly and only blow up at first
///    load — the "failed to compile lazy-loaded WASM module" prod break. Compiling here turns that
///    into an install-time rollback trigger.
pub async fn verify_install_runtime_ready(
    platform: &PlatformState,
    ps: &dyn temper_server::platform_store::PlatformStore,
    tenant: &str,
    app_name: &str,
) -> bool {
    let outcome = recover_installed_app_runtime_state(platform, ps, tenant, app_name).await;
    if !matches!(
        outcome,
        InstalledAppRuntimeRecoveryOutcome::Ready | InstalledAppRuntimeRecoveryOutcome::Healed
    ) {
        tracing::warn!(
            tenant,
            app = %app_name,
            outcome = ?outcome,
            "Genesis install verification: app is not runtime-ready"
        );
        return false;
    }

    let Some(bundle) = get_os_app(app_name) else {
        tracing::warn!(
            tenant,
            app = %app_name,
            "Genesis install verification: bundle missing from catalog"
        );
        return false;
    };
    required_wasm_modules_compile(
        &platform.server.wasm_engine,
        &bundle.wasm_modules,
        &bundle.wasm_module_configs,
    )
}

/// True iff every **required** wasm module (`criticality != optional`) is present and compiles.
///
/// Optional modules are intentionally skipped: a stray or optional `.wasm` must never fail an
/// otherwise-good install. Pure over its inputs (a wasm engine plus the bundle's module bytes and
/// declared contracts) so it is unit-testable without the app catalog or storage.
fn required_wasm_modules_compile(
    engine: &temper_wasm::WasmEngine,
    wasm_modules: &BTreeMap<String, Vec<u8>>,
    wasm_module_configs: &BTreeMap<String, WasmModuleManifest>,
) -> bool {
    for (module_name, config) in wasm_module_configs {
        if !config.is_required() {
            continue;
        }
        let Some(bytes) = wasm_modules.get(module_name) else {
            tracing::warn!(
                module = %module_name,
                "Genesis install verification: required wasm module declared but absent"
            );
            return false;
        };
        if let Err(error) = engine.compile_and_cache(bytes) {
            tracing::warn!(
                module = %module_name,
                error = %error,
                "Genesis install verification: required wasm module failed to compile"
            );
            return false;
        }
    }
    true
}

/// The durable platform store, if this instance has one wired.
pub(crate) fn platform_store(
    platform: &PlatformState,
) -> Option<std::sync::Arc<dyn temper_server::platform_store::PlatformStore>> {
    platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
}

/// Re-reconcile the prior bundle and restore its provenance record — the network-free core of
/// rollback, and the seam the deterministic simulator drives.
///
/// `reconcile_os_app` overwrites the durable record with a local-provenance row, so after
/// re-reconciling the prior bundle this restores the prior Genesis provenance record. If the prior
/// version itself does not reach runtime-ready, that is a hard both-broken error: neither the new
/// nor the previous version is serviceable.
///
/// Public for the deterministic simulator (`dst_genesis_install_rollback`), which drives this exact
/// production rollback effect against the simulated platform store under injected faults.
pub async fn restore_prior_install(
    platform: &PlatformState,
    ps: &dyn temper_server::platform_store::PlatformStore,
    tenant: &str,
    prior: &InstalledAppRecord,
) -> Result<(), String> {
    reconcile_materialized_app_closure(platform, tenant, &prior.app_name)
        .await
        .map_err(|error| {
            format!(
                "rollback re-reconcile of {} failed: {error}",
                prior.app_name
            )
        })?;
    if !verify_install_runtime_ready(platform, ps, tenant, &prior.app_name).await {
        return Err(format!(
            "rollback target {} for app {} is also not runtime-ready (both new and previous versions are broken)",
            prior.app_ref, prior.app_name
        ));
    }
    // Do not label whatever the catalog just reconciled with the prior record's digest unless it
    // actually IS the prior bundle. A corrupted cache or a concurrent catalog replacement could
    // reconcile different (ready) bytes; restoring the old digest onto them would be a lie.
    if !prior.bundle_digest.is_empty() {
        match os_app_bundle_digest(&prior.app_name).map(|digest| digest.bundle_digest) {
            Some(reconciled) if reconciled == prior.bundle_digest => {}
            other => {
                return Err(format!(
                    "rollback of {} reconciled a bundle whose digest ({:?}) does not match the previous record ({}); refusing to mislabel it",
                    prior.app_name, other, prior.bundle_digest
                ));
            }
        }
    }
    ps.record_installed_app_metadata(prior)
        .await
        .map_err(|error| {
            format!(
                "failed to restore previous install record for {}: {error}",
                prior.app_name
            )
        })?;
    Ok(())
}

/// Mark an installed-app record `failed` so nothing treats a version that did not reach
/// runtime-ready as healthy. Used when an install fails verification and there is no prior version
/// to roll back to (`reconcile_os_app` leaves the record `status = "installed"`).
pub(crate) async fn mark_install_failed(
    ps: &dyn temper_server::platform_store::PlatformStore,
    tenant: &str,
    app_name: &str,
) {
    match ps.get_installed_app(tenant, app_name).await {
        Ok(Some(mut record)) => {
            record.status = "failed".to_string();
            if let Err(error) = ps.record_installed_app_metadata(&record).await {
                tracing::warn!(
                    tenant,
                    app = %app_name,
                    error = %error,
                    "Failed to mark install record failed after verification failure"
                );
            }
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(
            tenant,
            app = %app_name,
            error = %error,
            "Could not read install record to mark it failed after verification failure"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn genesis_installed_record(app_ref: &str) -> InstalledAppRecord {
        InstalledAppRecord {
            source_kind: "genesis".to_string(),
            status: "installed".to_string(),
            app_ref: app_ref.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn classify_install_verify_commits_when_new_version_ready() {
        assert_eq!(
            classify_install_verify(true, None),
            InstallVerifyDecision::Commit
        );
        assert_eq!(
            classify_install_verify(true, Some(&genesis_installed_record("owner/app@old"))),
            InstallVerifyDecision::Commit
        );
    }

    #[test]
    fn classify_install_verify_rolls_back_to_prior_genesis_install() {
        // A failed new version with a prior good Genesis install reverts to it.
        assert_eq!(
            classify_install_verify(false, Some(&genesis_installed_record("owner/app@old"))),
            InstallVerifyDecision::RollBackToPrevious
        );
    }

    #[test]
    fn classify_install_verify_fails_clean_without_a_safe_prior() {
        // No prior at all -> nothing to revert to.
        assert_eq!(
            classify_install_verify(false, None),
            InstallVerifyDecision::FailNoRollback
        );
        // A prior that is not a completed Genesis install is not a safe last-good state.
        assert_eq!(
            classify_install_verify(
                false,
                Some(&InstalledAppRecord {
                    source_kind: "local".to_string(),
                    status: "installed".to_string(),
                    ..Default::default()
                })
            ),
            InstallVerifyDecision::FailNoRollback
        );
        assert_eq!(
            classify_install_verify(
                false,
                Some(&InstalledAppRecord {
                    source_kind: "genesis".to_string(),
                    status: "failed".to_string(),
                    ..Default::default()
                })
            ),
            InstallVerifyDecision::FailNoRollback
        );
    }

    #[test]
    fn required_wasm_modules_compile_rejects_broken_required_module() {
        use crate::os_apps::WasmModuleCriticality;
        let engine = temper_wasm::WasmEngine::default();
        // Minimal valid wasm module: magic + version header.
        let good: Vec<u8> = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        let broken: Vec<u8> = vec![0xde, 0xad, 0xbe, 0xef];

        let manifest = |criticality: WasmModuleCriticality| WasmModuleManifest {
            name: "m".to_string(),
            target: None,
            criticality,
            startup_loading: Default::default(),
            provenance: None,
            import_class: None,
        };
        let modules = |bytes: &Vec<u8>| {
            let mut m = BTreeMap::new();
            m.insert("m".to_string(), bytes.clone());
            m
        };
        let configs = |criticality: WasmModuleCriticality| {
            let mut c = BTreeMap::new();
            c.insert("m".to_string(), manifest(criticality));
            c
        };

        // Required (app or platform) + good bytes -> passes.
        assert!(required_wasm_modules_compile(
            &engine,
            &modules(&good),
            &configs(WasmModuleCriticality::AppRequired)
        ));
        // Required + broken bytes -> fails (this is the prod bug the probe catches).
        assert!(!required_wasm_modules_compile(
            &engine,
            &modules(&broken),
            &configs(WasmModuleCriticality::AppRequired)
        ));
        assert!(!required_wasm_modules_compile(
            &engine,
            &modules(&broken),
            &configs(WasmModuleCriticality::PlatformRequired)
        ));
        // Optional + broken bytes -> passes (optional modules never fail an install).
        assert!(required_wasm_modules_compile(
            &engine,
            &modules(&broken),
            &configs(WasmModuleCriticality::Optional)
        ));
        // Required but the module bytes are absent -> fails.
        assert!(!required_wasm_modules_compile(
            &engine,
            &BTreeMap::new(),
            &configs(WasmModuleCriticality::AppRequired)
        ));
    }
}
