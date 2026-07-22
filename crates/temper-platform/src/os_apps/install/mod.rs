//! OS-app installation preflight and publication entry points.

use super::*;

mod finalization;
mod publication_intent;
mod uploaded_wasm;
mod wasm_activation;

pub(super) use finalization::finalize_os_app_publication;
pub(super) use publication_intent::append_installed_app_record_intent;
pub(super) use uploaded_wasm::{UploadedWasmReplacementContext, uploaded_wasm_replacement_context};
pub(super) use wasm_activation::activate_wasm_generation;

/// Install an OS app into a tenant (workspace).
///
/// Reads app files from disk, runs the verification cascade, registers
/// specs in the SpecRegistry, loads Cedar policies, and persists everything
/// to the platform DB so specs survive redeployments.
///
/// Durable state is written before in-memory activation. A persistence error
/// therefore leaves the registry and Cedar engine on the prior generation.
pub async fn install_os_app(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<InstallResult, String> {
    let order = resolve_os_app_install_order(&[app_name.to_string()])?;

    let mut final_result = None;
    for app in order {
        let result = install_os_app_without_dependencies(state, tenant, &app).await?;
        if app == app_name {
            final_result = Some(result);
        }
    }
    final_result.ok_or_else(|| format!("OS app '{app_name}' produced no install result"))
}

async fn install_os_app_without_dependencies(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<InstallResult, String> {
    install_os_app_with_plan(state, tenant, app_name, OsAppInstallPlan::all()).await
}

pub(super) fn bundled_wasm_replaces_existing(
    module_name: &str,
    wasm_bytes: &[u8],
    bundle: &AppBundle,
    existing_sources: &BTreeMap<String, WasmModuleSource>,
    upload_replacement: &UploadedWasmReplacementContext,
) -> bool {
    let hash = temper_wasm::WasmEngine::hash_module(wasm_bytes);
    let required = bundle
        .wasm_module_configs
        .get(module_name)
        .is_some_and(WasmModuleManifest::is_required);
    existing_sources.get(module_name).is_some_and(|existing| {
        upload_replacement.should_replace(existing)
            || (required && existing.source == "upload" && existing.sha256_hash != hash)
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "publication validation receives the complete preflight generation"
)]
pub(super) fn validate_os_app_publication_candidate(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    plan: OsAppInstallPlan,
    bundle: &AppBundle,
    complete_specs: &BTreeMap<String, String>,
    complete_csdl: Option<&str>,
    complete_constraints: Option<&str>,
    complete_policy: &str,
    existing_sources: &BTreeMap<String, WasmModuleSource>,
    upload_replacement: &UploadedWasmReplacementContext,
) -> Result<(), String> {
    temper_authz::AuthzEngine::new(complete_policy).map_err(|error| {
        format!("Invalid merged Cedar policies for os-app '{app_name}': {error}")
    })?;

    if !complete_specs.is_empty() {
        let csdl_source = complete_csdl.ok_or_else(|| {
            format!("OS app '{app_name}' publishes specs without a complete tenant CSDL")
        })?;
        let csdl = parse_csdl(csdl_source)
            .map_err(|error| format!("Invalid merged CSDL for os-app '{app_name}': {error}"))?;
        let specs = complete_specs
            .iter()
            .map(|(entity_type, source)| (entity_type.as_str(), source.as_str()))
            .collect::<Vec<_>>();
        let mut candidate_registry = temper_server::registry::SpecRegistry::new();
        candidate_registry
            .try_register_tenant_with_reactions_constraints_and_key_epochs(
                TenantId::new(tenant),
                csdl,
                csdl_source.to_string(),
                &specs,
                Vec::new(),
                complete_constraints.map(str::to_string),
                false,
                &BTreeMap::new(),
            )
            .map_err(|error| {
                format!("Invalid complete spec generation for os-app '{app_name}': {error}")
            })?;
    } else if let Some(source) = complete_constraints {
        temper_spec::cross_invariant::parse_cross_invariants(source).map_err(|error| {
            format!("Invalid cross-invariants for os-app '{app_name}': {error}")
        })?;
    }

    if plan.wasm {
        for (module_name, config) in &bundle.wasm_module_configs {
            if config.is_required() && !bundle.wasm_modules.contains_key(module_name) {
                return Err(format!(
                    "Required WASM module '{module_name}' is missing from os-app '{app_name}'"
                ));
            }
        }
        for (module_name, wasm_bytes) in &bundle.wasm_modules {
            let hash = temper_wasm::WasmEngine::hash_module(wasm_bytes);
            let preserve_upload = existing_sources.get(module_name).is_some_and(|existing| {
                existing.source == "upload"
                    && existing.sha256_hash != hash
                    && !bundled_wasm_replaces_existing(
                        module_name,
                        wasm_bytes,
                        bundle,
                        existing_sources,
                        upload_replacement,
                    )
            });
            let config = bundle.wasm_module_configs.get(module_name);
            let must_compile = config.is_some_and(|config| {
                config.is_required() || config.startup_loading == WasmStartupLoading::Eager
            });
            if must_compile && !preserve_upload {
                state
                    .server
                    .wasm_engine
                    .compile_and_cache(wasm_bytes)
                    .map_err(|error| {
                        format!(
                            "Invalid required/eager WASM module '{module_name}' in os-app '{app_name}': {error}"
                        )
                    })?;
            }
        }
    }
    Ok(())
}

pub(super) async fn install_os_app_with_plan(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    plan: OsAppInstallPlan,
) -> Result<InstallResult, String> {
    let tenant_id = TenantId::new(tenant);
    let mut publication_guard = state.server.begin_spec_publication(&tenant_id).await?;
    install_os_app_with_plan_under_guard(
        state,
        tenant,
        app_name,
        plan,
        None,
        &mut publication_guard,
    )
    .await
}
