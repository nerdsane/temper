//! Store and register an app bundle's WASM modules during installation.

use std::time::Instant;

use futures::future::join_all;
use temper_runtime::tenant::TenantId;

use super::wasm_replacement::UploadedWasmReplacementContext;
use super::{AppBundle, WasmModuleCriticality, WasmModuleManifest, WasmStartupLoading};
use crate::state::PlatformState;

/// Persist and register the bundle's WASM modules, warming only eager ones:
/// the modules it registered, skipped and failed. Source-aware preservation:
/// if a (tenant, module) row in the durable store has source='upload' (a hot
/// upload from `POST /api/wasm/modules/{name}`), uploads newer than the
/// installed app record are kept. Uploads are replaced when the bundle digest
/// changed or durable metadata predates the app record.
pub(super) async fn register_bundle_modules(
    state: &PlatformState,
    tenant: &str,
    tenant_id: &TenantId,
    bundle: &AppBundle,
    upload_replacement: &UploadedWasmReplacementContext,
) -> BundleModules {
    let mut wasm_registered = Vec::new();
    let mut wasm_skipped = Vec::new();
    let mut wasm_failures = Vec::new();
    let existing_sources = match state.server.load_wasm_module_sources(tenant).await {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(
                tenant,
                error = %e,
                "Failed to load existing WASM modules; install pipeline will not honor hot-upload preservation this cycle"
            );
            std::collections::BTreeMap::new()
        }
    };

    let mut to_store = Vec::with_capacity(bundle.wasm_modules.len());
    for (module_name, wasm_bytes) in &bundle.wasm_modules {
        let module_config = bundle.wasm_module_configs.get(module_name);
        let hash = temper_wasm::WasmEngine::hash_module(wasm_bytes);
        let required = module_config.is_some_and(WasmModuleManifest::is_required);
        let replace_uploaded_module = existing_sources.get(module_name).is_some_and(|existing| {
            upload_replacement.should_replace(existing)
                || (required && existing.source == "upload" && existing.sha256_hash != hash)
        });
        if let Some(existing) = existing_sources.get(module_name)
            && existing.source == "upload"
            && existing.sha256_hash != hash
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
                wasm_skipped.push(module_name.clone());
                continue;
            }
        }
        to_store.push((module_name, wasm_bytes, hash, replace_uploaded_module));
    }

    // All at once: each one asks the object store whether it holds these bytes,
    // about 100 ms a module on an app's Temper and most of them unchanged, so
    // asked in turn Arni Labs' 12 take 1.2 s. Asking is what repairs a lost object.
    let stored = join_all(to_store.iter().map(
        |(module_name, wasm_bytes, hash, replace_uploaded_module)| async move {
            let started = Instant::now();
            let result = if *replace_uploaded_module {
                state
                    .server
                    .upsert_bundled_wasm_module_replacing_upload(
                        tenant,
                        module_name,
                        wasm_bytes,
                        hash,
                    )
                    .await
            } else {
                state
                    .server
                    .upsert_wasm_module(tenant, module_name, wasm_bytes, hash, "bundled")
                    .await
            };
            (result, started.elapsed())
        },
    ))
    .await;

    for ((module_name, wasm_bytes, hash, _), (upsert_result, stored_in)) in
        to_store.iter().zip(stored)
    {
        let module_config = bundle.wasm_module_configs.get(*module_name);
        if let Err(e) = upsert_result {
            tracing::warn!(
                tenant,
                module = %module_name,
                error = %e,
                "Failed to persist WASM module to durable store (continuing in-memory only)"
            );
        }
        state
            .server
            .register_wasm_module(tenant_id, module_name, hash);

        if matches!(
            module_config.map(|config| config.startup_loading),
            Some(WasmStartupLoading::Eager)
        ) && let Err(e) = state.server.wasm_engine.compile_and_cache(wasm_bytes)
        {
            wasm_failures.push((*module_name).clone());
            tracing::warn!(
                tenant,
                module = %module_name,
                error = %e,
                "Failed to eagerly compile WASM module from OS app"
            );
        }

        tracing::info!(
            tenant,
            module = %module_name,
            hash = %hash,
            size = wasm_bytes.len(),
            duration_ms = stored_in.as_millis() as u64,
            startup_loading = ?module_config
                .map(|config| config.startup_loading)
                .unwrap_or_default(),
            "WASM module registered from OS app"
        );
        wasm_registered.push((*module_name).clone());
    }

    for (module_name, module_config) in &bundle.wasm_module_configs {
        if bundle.wasm_modules.contains_key(module_name) {
            continue;
        }
        match module_config.criticality {
            WasmModuleCriticality::Optional => {
                wasm_skipped.push(module_name.clone());
                tracing::warn!(
                    tenant,
                    module = %module_name,
                    "Configured optional WASM module artifact is missing from the app bundle"
                );
            }
            WasmModuleCriticality::PlatformRequired | WasmModuleCriticality::AppRequired => {
                wasm_failures.push(module_name.clone());
                tracing::error!(
                    tenant,
                    module = %module_name,
                    criticality = ?module_config.criticality,
                    "Configured required WASM module artifact is missing from the app bundle"
                );
            }
        }
    }
    BundleModules {
        registered: wasm_registered,
        skipped: wasm_skipped,
        failures: wasm_failures,
    }
}

/// What [`register_bundle_modules`] did with each of a bundle's modules.
#[derive(Default)]
pub(super) struct BundleModules {
    pub(super) registered: Vec<String>,
    pub(super) skipped: Vec<String>,
    pub(super) failures: Vec<String>,
}
