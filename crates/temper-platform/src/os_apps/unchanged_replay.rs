//! A boot's replay of an install whose bundle has not changed since it was
//! recorded.
//!
//! Before the replay, the boot has already loaded what that install stored:
//! its specs, its Cedar and its modules' hashes. Installing it again would
//! write the same rows and bootstrap the same content, once more for every
//! recorded app that depends on it. Such a replay writes nothing; it redoes
//! only what an install keeps in memory alone, which the boot does not load.

use super::reconcile::{digest_app_bundle, tenant_has_durable_wasm_for_bundle};
use super::{
    AppBundle, AppDeploymentMode, InstallResult, WasmStartupLoading, load_tenant_os_app,
    tenant_has_active_policies_for_bundle, tenant_has_ready_app_specs_for_bundle,
    tenant_has_registered_wasm_for_bundle,
};
use crate::state::PlatformState;

/// The replay of `app_name` into `tenant` when its bundle is the one its
/// install record names and the boot already serves all of it: its specs
/// verified, its Cedar active and its modules registered and stored. None
/// when any of that does not hold, and the app is installed again.
pub(super) async fn registered(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Option<InstallResult> {
    // An unavailable tenant is refused by the installer, with its reason.
    if state.tenant_unavailable_reason(tenant).is_some() {
        return None;
    }
    let store = state.server.storage_stack.as_ref()?.platform.clone()?;
    let bundle = load_tenant_os_app(tenant, app_name).ok()?;
    let record = store.get_installed_app(tenant, app_name).await.ok()??;
    let digest = digest_app_bundle(Some(tenant), app_name, &bundle);
    if record.bundle_digest.is_empty() || record.bundle_digest != digest.bundle_digest {
        return None;
    }
    let served = tenant_has_ready_app_specs_for_bundle(state, tenant, &bundle)
        && tenant_has_active_policies_for_bundle(state, tenant, &bundle)
        && tenant_has_registered_wasm_for_bundle(state, tenant, &bundle)
        && tenant_has_durable_wasm_for_bundle(state, tenant, &bundle).await;
    if !served {
        return None;
    }

    if bundle.deployment_mode == AppDeploymentMode::Commons {
        state.server.enable_commons_guardrails(tenant);
    }
    let wasm_failures = compile_eager_modules(state, tenant, &bundle);
    tracing::info!(
        tenant,
        app = %app_name,
        bundle_digest = %digest.bundle_digest,
        "os-app unchanged since its install; registered without installing again"
    );
    Some(InstallResult {
        skipped: bundle
            .specs
            .iter()
            .map(|(entity, _)| entity.clone())
            .collect(),
        wasm_modules: bundle.wasm_modules.keys().cloned().collect(),
        wasm_failures,
        ..InstallResult::default()
    })
}

/// Compile the bundle's eager modules, as an install warms them: the boot
/// registers every module's hash and compiles none. Returns those that failed.
fn compile_eager_modules(state: &PlatformState, tenant: &str, bundle: &AppBundle) -> Vec<String> {
    let mut failures = Vec::new();
    for (module_name, wasm_bytes) in &bundle.wasm_modules {
        let eager = bundle
            .wasm_module_configs
            .get(module_name)
            .is_some_and(|config| matches!(config.startup_loading, WasmStartupLoading::Eager));
        if !eager {
            continue;
        }
        if let Err(error) = state.server.wasm_engine.compile_and_cache(wasm_bytes) {
            tracing::warn!(
                tenant,
                module = %module_name,
                error = %error,
                "Failed to eagerly compile WASM module from OS app"
            );
            failures.push(module_name.clone());
        }
    }
    failures
}
