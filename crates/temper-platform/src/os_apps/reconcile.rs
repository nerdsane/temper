use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;
use temper_server::platform_store::InstalledAppRecord;
use temper_wasm_sdk::data::ModuleSdkManifest;
use temper_wasm_sdk::schema_deployment::StreamDescriptorMigrationTargetV1;

use super::{
    AppBundle, OsAppBundleDigest, OsAppInstallPlan, OsAppReconcileResult, catalog,
    digest_app_bundle_with_version, install_os_app_from_dir_with_plan, load_app_bundle,
    read_app_manifest, restore_app_specs_from_matching_digest,
    tenant_has_ready_app_specs_for_bundle,
};
use crate::state::PlatformState;

mod order;
mod stream_contract;
pub use order::resolve_os_app_install_order;
#[cfg(test)]
pub(super) use order::resolve_os_app_install_order_with_dependencies;

pub(super) fn plan_reconcile_from_installed_record(
    record: &InstalledAppRecord,
    digest: &OsAppBundleDigest,
    specs_ready: bool,
    wasm_registered: bool,
    policies_active: bool,
) -> OsAppInstallPlan {
    OsAppInstallPlan {
        specs: record.spec_digest != digest.spec_digest || !specs_ready,
        policies: record.policy_digest != digest.policy_digest || !policies_active,
        wasm: record.wasm_digest != digest.wasm_digest || !wasm_registered,
        content: record.content_digest != digest.content_digest,
        seed: record.seed_digest != digest.seed_digest,
    }
}

pub(crate) fn tenant_has_active_policies_for_bundle(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
) -> bool {
    if bundle.cedar_policies.is_empty() {
        return true;
    }

    let Some(active_text) = state.server.authz.get_tenant_policy_text(tenant) else {
        return false;
    };

    bundle_policies_present(&active_text, &bundle.cedar_policies)
}

fn bundle_policies_present(active_text: &str, cedar_policies: &[String]) -> bool {
    cedar_policies.iter().all(|policy| {
        let policy = policy.trim();
        policy.is_empty() || active_text.contains(policy)
    })
}

fn bundle_required_wasm_artifacts_present(bundle: &AppBundle) -> bool {
    bundle
        .wasm_module_configs
        .iter()
        .all(|(module_name, config)| {
            !config.is_required() || bundle.wasm_modules.contains_key(module_name)
        })
}

pub(crate) fn tenant_has_registered_wasm_for_bundle(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
) -> bool {
    if !bundle_required_wasm_artifacts_present(bundle) {
        return false;
    }
    let tenant_id = TenantId::new(tenant);
    let registry = state
        .server
        .wasm_module_registry
        .read()
        .expect("WASM module registry lock poisoned");
    bundle.wasm_modules.iter().all(|(module_name, wasm_bytes)| {
        let hash = temper_wasm::WasmEngine::hash_module(wasm_bytes);
        registry.get_hash(&tenant_id, module_name) == Some(hash.as_str())
    })
}

pub(super) fn restore_canonical_data_bindings(
    state: &PlatformState,
    tenant: &str,
    wasm_modules: &BTreeMap<String, Vec<u8>>,
    canonical_bindings: &BTreeMap<String, ModuleSdkManifest>,
) -> Result<(), String> {
    let tenant_id = TenantId::new(tenant);
    let mut registry = state
        .server
        .wasm_module_registry
        .write()
        .map_err(|_| "WASM module registry lock poisoned".to_string())?;

    for (module_name, binding) in canonical_bindings {
        let wasm_bytes = wasm_modules.get(module_name).ok_or_else(|| {
            format!("module '{module_name}' canonical data binding has no WASM artifact")
        })?;
        let artifact_digest = temper_wasm::WasmEngine::hash_module(wasm_bytes);
        if registry.get_hash(&tenant_id, module_name) != Some(artifact_digest.as_str()) {
            return Err(format!(
                "module '{module_name}' canonical data binding does not match the registered artifact"
            ));
        }
        registry.bind_data_manifest(&tenant_id, module_name, &artifact_digest, binding.clone());
    }

    Ok(())
}

pub(crate) async fn tenant_has_durable_wasm_for_bundle(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
) -> bool {
    if !bundle_required_wasm_artifacts_present(bundle) {
        return false;
    }
    if bundle.wasm_modules.is_empty() {
        return true;
    }
    let sources = match state.server.load_wasm_module_sources(tenant).await {
        Ok(sources) => sources,
        Err(error) => {
            tracing::warn!(
                tenant,
                error = %error,
                "Failed to load durable WASM module metadata during os-app reconcile"
            );
            return false;
        }
    };
    bundle.wasm_modules.iter().all(|(module_name, wasm_bytes)| {
        let hash = temper_wasm::WasmEngine::hash_module(wasm_bytes);
        sources
            .get(module_name)
            .map(|source| source.sha256_hash.as_str())
            == Some(hash.as_str())
    })
}

async fn record_app_install_metadata(
    state: &PlatformState,
    tenant: &str,
    digest: &OsAppBundleDigest,
    status: &str,
) {
    let Some(ps) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return;
    };

    let record = InstalledAppRecord {
        tenant: tenant.to_string(),
        app_name: digest.app_name.clone(),
        source_kind: "local".to_string(),
        app_ref: String::new(),
        version_hash: String::new(),
        pinned_version_hash: String::new(),
        current_version_hash: String::new(),
        follow_policy: "pinned".to_string(),
        closure_id: String::new(),
        registry_url: String::new(),
        registry_tenant: String::new(),
        dependency_lock_digest: String::new(),
        app_version: digest.app_version.clone(),
        bundle_digest: digest.bundle_digest.clone(),
        spec_digest: digest.spec_digest.clone(),
        policy_digest: digest.policy_digest.clone(),
        wasm_digest: digest.wasm_digest.clone(),
        content_digest: digest.content_digest.clone(),
        seed_digest: digest.seed_digest.clone(),
        installed_at: None,
        last_reconciled_at: None,
        status: status.to_string(),
    };

    if let Err(error) = ps.record_installed_app_metadata(&record).await {
        tracing::warn!(
            tenant,
            app = %digest.app_name,
            error = %error,
            "Failed to persist OS app digest metadata"
        );
    }
}

pub(super) async fn record_app_install_metadata_for_bundle_version(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    app_version: &str,
    app_guide: Option<&str>,
    bundle: &AppBundle,
) {
    let bundle_digest = digest_app_bundle_with_version(app_name, app_version, app_guide, bundle);
    record_app_install_metadata(state, tenant, &bundle_digest, "installed").await;
}

/// Reconcile one app without recursively processing dependencies.
///
/// Callers that need dependencies should first call
/// [`resolve_os_app_install_order`] and reconcile each returned app once.
pub async fn reconcile_os_app(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<OsAppReconcileResult, String> {
    let app_dir = {
        let catalog = catalog()
            .read()
            .map_err(|_| "OS app catalog lock poisoned")?;
        catalog
            .paths
            .get(app_name)
            .cloned()
            .ok_or_else(|| format!("OS app '{app_name}' not found"))?
    };
    reconcile_os_app_from_dir(state, tenant, app_name, &app_dir, None).await
}

/// Reconcile one immutable app directory without consulting the global catalog.
pub(crate) async fn reconcile_os_app_from_dir(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    app_dir: &std::path::Path,
    canonical_bundle: Option<(
        &crate::app_bundles::CanonicalBundleManifestV1,
        &std::path::Path,
    )>,
) -> Result<OsAppReconcileResult, String> {
    let manifest = read_app_manifest(app_dir)
        .ok_or_else(|| format!("OS app '{app_name}' has no valid app.toml"))?;
    if manifest.name != app_name {
        return Err(format!(
            "OS app directory declares '{}' but '{}' was requested",
            manifest.name, app_name
        ));
    }
    let bundle = load_app_bundle(app_dir).ok_or_else(|| {
        format!(
            "OS app '{app_name}' at '{}' failed to load",
            app_dir.display()
        )
    })?;
    let app_guide = std::fs::read_to_string(app_dir.join("APP.md")).ok();
    let digest =
        digest_app_bundle_with_version(app_name, &manifest.version, app_guide.as_deref(), &bundle);
    let canonical_bindings = canonical_bundle
        .map(|(manifest, materialized_view)| {
            super::data_binding::verify_bundle_data_bindings(
                &bundle,
                app_name,
                manifest,
                materialized_view,
            )
        })
        .transpose()?;

    let (stream_contract_capability_digest, stream_contract_fence_already_active) =
        match stream_contract::gate(state, tenant, app_name, &digest, &bundle).await? {
            stream_contract::Gate::Ready {
                capability_digest,
                fence_already_active,
            } => (capability_digest, fence_already_active),
            stream_contract::Gate::MigrationRequired(result) => return Ok(result),
        };

    let post_reconcile_digest = digest.clone();
    let reconcile_result: Result<OsAppReconcileResult, String> = async {
        if let Some(ps) = state
            .server
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.platform.clone())
        {
            match ps.get_installed_app(tenant, app_name).await {
                Ok(Some(record)) => {
                    let mut specs_ready =
                        tenant_has_ready_app_specs_for_bundle(state, tenant, &bundle);
                    let wasm_registered =
                        tenant_has_registered_wasm_for_bundle(state, tenant, &bundle);
                    let durable_wasm_ready =
                        tenant_has_durable_wasm_for_bundle(state, tenant, &bundle).await;
                    let wasm_ready = wasm_registered && durable_wasm_ready;
                    let policies_active =
                        tenant_has_active_policies_for_bundle(state, tenant, &bundle);

                    if wasm_ready && let Some(bindings) = canonical_bindings.as_ref() {
                        restore_canonical_data_bindings(
                            state,
                            tenant,
                            &bundle.wasm_modules,
                            bindings,
                        )?;
                    }

                    if record.bundle_digest == digest.bundle_digest && !specs_ready {
                        specs_ready = restore_app_specs_from_matching_digest(
                            state,
                            ps.as_ref(),
                            tenant,
                            app_name,
                            &bundle,
                        )
                        .await;
                    }

                    if record.bundle_digest == digest.bundle_digest
                        && specs_ready
                        && wasm_ready
                        && policies_active
                    {
                        tracing::info!(
                            tenant,
                            app = %app_name,
                            bundle_digest = %digest.bundle_digest,
                            "OS app unchanged; skipping hot reconcile"
                        );
                        return Ok(OsAppReconcileResult::Skipped {
                            app_name: app_name.to_string(),
                            bundle_digest: digest.bundle_digest,
                        });
                    }

                    if !specs_ready && record.spec_digest == digest.spec_digest {
                        specs_ready = restore_app_specs_from_matching_digest(
                            state,
                            ps.as_ref(),
                            tenant,
                            app_name,
                            &bundle,
                        )
                        .await;
                    }
                    let plan = plan_reconcile_from_installed_record(
                        &record,
                        &digest,
                        specs_ready,
                        wasm_ready,
                        policies_active,
                    );

                    tracing::info!(
                        tenant,
                        app = %app_name,
                        bundle_digest = %digest.bundle_digest,
                        specs = plan.specs,
                        policies = plan.policies,
                        wasm = plan.wasm,
                        content = plan.content,
                        seed = plan.seed,
                        "OS app changed; running delta reconcile"
                    );
                    let install = install_os_app_from_dir_with_plan(
                        state,
                        tenant,
                        app_name,
                        app_dir,
                        plan,
                        canonical_bindings.as_ref(),
                    )
                    .await?;
                    return Ok(OsAppReconcileResult::Installed {
                        app_name: app_name.to_string(),
                        bundle_digest: digest.bundle_digest,
                        install: Box::new(install),
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        tenant,
                        app = %app_name,
                        error = %error,
                        "Failed to read OS app digest metadata; falling back to reconcile"
                    );
                }
            }
        }

        let install = install_os_app_from_dir_with_plan(
            state,
            tenant,
            app_name,
            app_dir,
            OsAppInstallPlan::all(),
            canonical_bindings.as_ref(),
        )
        .await?;
        Ok(OsAppReconcileResult::Installed {
            app_name: app_name.to_string(),
            bundle_digest: digest.bundle_digest,
            install: Box::new(install),
        })
    }
    .await;
    match reconcile_result {
        Ok(result) => {
            if let Some(capability_digest) = stream_contract_capability_digest.as_ref()
                && !stream_contract_fence_already_active
            {
                let target = StreamDescriptorMigrationTargetV1::InstalledApplication {
                    application_id: app_name.into(),
                    semantic_digest: post_reconcile_digest.spec_digest.clone(),
                };
                let fence = match state
                    .server
                    .require_stream_descriptor_completion_v1(&TenantId::new(tenant), &target, None)
                    .await
                {
                    Ok(Some(fence)) => fence,
                    Err(error)
                        if error.starts_with("backend unavailable:")
                            || error.starts_with("stale fence:") =>
                    {
                        return Err(error);
                    }
                    Ok(None) | Err(_) => {
                        return Ok(OsAppReconcileResult::MigrationRequired {
                            app_name: app_name.into(),
                            semantic_digest: post_reconcile_digest.spec_digest,
                            capability_digest: capability_digest.clone(),
                            descriptor_contract_version: 1,
                        });
                    }
                };
                state
                    .server
                    .activate_installed_application_stream_fence_v1(&TenantId::new(tenant), &fence)
                    .await?;
            } else if stream_contract_capability_digest.is_none() {
                state
                    .server
                    .deactivate_installed_application_stream_fence_v1(
                        &TenantId::new(tenant),
                        app_name,
                        None,
                    )
                    .await?;
            }
            Ok(result)
        }
        Err(error) => Err(error),
    }
}
/// ARN-68: after a reconcile registers changed specs, re-key any type whose declared
/// key-set changed (a newly-declared `[[key]]` on an already-installed type, e.g.
/// Directory `ws_path` added after `name_parent`). The once-per-boot startup key-index
/// backfill can fire BEFORE this (late — post-boot in prod) reconcile registers the key,
/// see only the old key-set, and skip — so the added key would never backfill for
/// existing entities and reads scan → 413. Running the key-set-aware re-key here, after
/// registration, closes that race; it only re-scans types whose key-set actually changed
/// (unchanged types skip via the watermark) and is spawned so a large re-key never blocks
/// the reconcile. See ADR-0153.
pub(super) fn spawn_key_index_rekey_after_spec_change(state: &PlatformState, tenant_id: &TenantId) {
    let server = state.server.clone();
    let tenant = tenant_id.clone();
    tokio::spawn(async move {
        server.populate_key_index_from_snapshots(&tenant).await;
        // ADR-0155: same race for a newly declared [[vector]] path — re-index existing
        // entities here (watermark-gated,
        // unchanged types skip) so they are immediately rankable by Temper.Nearest.
        server.populate_vector_index_from_snapshots(&tenant).await;
    });
}
