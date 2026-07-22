//! In-memory activation for the durable WASM generation selected at preflight.

use std::time::Instant;

use super::super::*;

pub(in crate::os_apps) struct WasmActivation {
    pub(in crate::os_apps) registered: Vec<String>,
    pub(in crate::os_apps) skipped: Vec<String>,
    pub(in crate::os_apps) failures: Vec<String>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "WASM activation consumes one complete validated installation generation"
)]
pub(in crate::os_apps) async fn activate_wasm_generation(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    plan: OsAppInstallPlan,
    bundle: &AppBundle,
    tenant_id: &TenantId,
    effective_wasm_hashes: &BTreeMap<String, String>,
    existing_sources: &BTreeMap<String, WasmModuleSource>,
    upload_replacement: &UploadedWasmReplacementContext,
) -> Result<WasmActivation, String> {
    let mut registered = Vec::new();
    let mut skipped = Vec::new();
    let mut failures = Vec::new();
    if plan.wasm {
        for (module_name, wasm_bytes) in &bundle.wasm_modules {
            let module_started = Instant::now();
            let module_config = bundle.wasm_module_configs.get(module_name);
            let bundled_hash = temper_wasm::WasmEngine::hash_module(wasm_bytes);
            let hash = effective_wasm_hashes
                .get(module_name)
                .cloned()
                .unwrap_or_else(|| bundled_hash.clone());
            let replace_uploaded_module = bundled_wasm_replaces_existing(
                module_name,
                wasm_bytes,
                bundle,
                existing_sources,
                upload_replacement,
            );
            if let Some(existing) = existing_sources.get(module_name)
                && existing.source == "upload"
                && existing.sha256_hash != bundled_hash
            {
                if replace_uploaded_module {
                    tracing::info!(
                        tenant,
                        module = %module_name,
                        bundled_hash = %hash,
                        upload_hash = %existing.sha256_hash,
                        "Replacing stale hot-uploaded WASM module during os-app reconcile"
                    );
                } else {
                    tracing::info!(
                        tenant,
                        module = %module_name,
                        bundled_hash = %hash,
                        upload_hash = %existing.sha256_hash,
                        "Skipping bundled install: hot-uploaded module preserved"
                    );
                    state
                        .server
                        .wasm_module_registry
                        .write()
                        .map_err(|error| {
                            format!("WASM registry lock poisoned during os-app install: {error}")
                        })?
                        .register(tenant_id, module_name, &hash);
                    skipped.push(module_name.clone());
                    continue;
                }
            }
            state
                .server
                .wasm_module_registry
                .write()
                .map_err(|error| {
                    format!("WASM registry lock poisoned during os-app install: {error}")
                })?
                .register(tenant_id, module_name, &hash);

            if matches!(
                module_config.map(|config| config.startup_loading),
                Some(WasmStartupLoading::Eager)
            ) && let Err(error) = state.server.wasm_engine.compile_and_cache(wasm_bytes)
            {
                failures.push(module_name.clone());
                tracing::warn!(
                    tenant,
                    module = %module_name,
                    error = %error,
                    "Failed to eagerly compile WASM module from OS app"
                );
            }

            tracing::info!(
                tenant,
                module = %module_name,
                hash = %hash,
                size = wasm_bytes.len(),
                duration_ms = module_started.elapsed().as_millis() as u64,
                startup_loading = ?module_config
                    .map(|config| config.startup_loading)
                    .unwrap_or_default(),
                "WASM module registered from OS app"
            );
            registered.push(module_name.clone());
        }

        for (module_name, module_config) in &bundle.wasm_module_configs {
            if bundle.wasm_modules.contains_key(module_name) {
                continue;
            }
            match module_config.criticality {
                WasmModuleCriticality::Optional => {
                    skipped.push(module_name.clone());
                    tracing::warn!(
                        tenant,
                        module = %module_name,
                        "Configured optional WASM module artifact is missing from the app bundle"
                    );
                }
                WasmModuleCriticality::PlatformRequired | WasmModuleCriticality::AppRequired => {
                    failures.push(module_name.clone());
                    tracing::error!(
                        tenant,
                        module = %module_name,
                        criticality = ?module_config.criticality,
                        "Configured required WASM module artifact is missing from the app bundle"
                    );
                }
            }
        }
    }

    if !failures.is_empty() {
        failures.sort();
        failures.dedup();
        return Err(format!(
            "WASM modules named by the os-app generation were not activated for '{app_name}': {}",
            failures.join(", ")
        ));
    }
    if bundle.deployment_mode == AppDeploymentMode::Commons {
        state.server.enable_commons_guardrails(tenant);
    }
    Ok(WasmActivation {
        registered,
        skipped,
        failures,
    })
}
