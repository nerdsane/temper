//! Genesis install verification and the pure verify/rollback decision.
//!
//! Extracted from `genesis_install.rs` to keep that file within the readability budget and to give
//! the verify+rollback contract (ARN-421) a cohesive home. The stateful rollback effects that need
//! the private materialize/reconcile helpers stay in `genesis_install.rs`; this module holds the
//! pieces that depend only on the catalog, the recovery probe, and the wasm engine.

use std::collections::BTreeMap;

use temper_server::platform_store::InstalledAppRecord;

use crate::os_apps::{WasmModuleManifest, get_os_app};
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
pub(crate) async fn verify_install_runtime_ready(
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
