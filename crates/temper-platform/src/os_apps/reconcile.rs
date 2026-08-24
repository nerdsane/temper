use std::collections::HashSet;

use temper_runtime::tenant::TenantId;
use temper_server::platform_store::InstalledAppRecord;

use super::{
    AppBundle, OsAppBundleDigest, OsAppInstallPlan, OsAppReconcileResult, catalog,
    digest_app_bundle_with_version, install_os_app_from_dir_with_plan, load_app_bundle,
    os_app_dependencies, read_app_manifest, restore_app_specs_from_matching_digest,
    tenant_has_ready_app_specs_for_bundle,
};
use crate::state::PlatformState;

fn collect_install_order_with_dependencies(
    app_name: &str,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    order: &mut Vec<String>,
    dependencies: &impl Fn(&str) -> Vec<String>,
) -> Result<(), String> {
    if visited.contains(app_name) {
        return Ok(());
    }
    if !visiting.insert(app_name.to_string()) {
        return Err(format!("Cyclic OS app dependency detected at '{app_name}'"));
    }
    for dependency in dependencies(app_name) {
        collect_install_order_with_dependencies(
            &dependency,
            visiting,
            visited,
            order,
            dependencies,
        )?;
    }
    visiting.remove(app_name);
    visited.insert(app_name.to_string());
    order.push(app_name.to_string());
    Ok(())
}

/// Resolve a deduplicated dependency-first install order for a set of apps.
pub fn resolve_os_app_install_order(app_names: &[String]) -> Result<Vec<String>, String> {
    resolve_os_app_install_order_with_dependencies(app_names, |app_name| {
        os_app_dependencies(app_name)
    })
}

pub(super) fn resolve_os_app_install_order_with_dependencies(
    app_names: &[String],
    dependencies: impl Fn(&str) -> Vec<String>,
) -> Result<Vec<String>, String> {
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    let mut order = Vec::new();
    for app_name in app_names {
        collect_install_order_with_dependencies(
            app_name,
            &mut visiting,
            &mut visited,
            &mut order,
            &dependencies,
        )?;
    }
    Ok(order)
}

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
    canonical_closure_id: Option<&str>,
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
    if let Some(closure_id) = canonical_closure_id {
        super::data_binding::verify_bundle_data_bindings(&bundle, closure_id)?;
    }

    if let Some(ps) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    {
        match ps.get_installed_app(tenant, app_name).await {
            Ok(Some(record)) => {
                let mut specs_ready = tenant_has_ready_app_specs_for_bundle(state, tenant, &bundle);
                let wasm_registered = tenant_has_registered_wasm_for_bundle(state, tenant, &bundle);
                let durable_wasm_ready =
                    tenant_has_durable_wasm_for_bundle(state, tenant, &bundle).await;
                let wasm_ready = wasm_registered && durable_wasm_ready;
                let policies_active = tenant_has_active_policies_for_bundle(state, tenant, &bundle);

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
                    canonical_closure_id,
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
        canonical_closure_id,
    )
    .await?;
    Ok(OsAppReconcileResult::Installed {
        app_name: app_name.to_string(),
        bundle_digest: digest.bundle_digest,
        install: Box::new(install),
    })
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
        // ADR-0155: same race for a newly declared [[vector]] path — a late reconcile
        // registers it after boot, so re-index existing entities here (watermark-gated,
        // unchanged types skip) so they are immediately rankable by Temper.Nearest.
        server.populate_vector_index_from_snapshots(&tenant).await;
    });
}
