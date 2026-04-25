use std::collections::HashSet;

use sha2::{Digest, Sha256};
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::{InstalledAppRecord, PlatformStore, SpecVerificationUpdate};
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_spec::csdl::parse_csdl;

use super::{
    AppBundle, AppEntry, OsAppBundleDigest, OsAppReconcileResult, catalog, get_os_app,
    install_os_app_without_dependencies, os_app_dependencies,
};
use crate::state::PlatformState;

pub(super) fn collect_install_order(
    app_name: &str,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    order: &mut Vec<String>,
) -> Result<(), String> {
    if visited.contains(app_name) {
        return Ok(());
    }
    if !visiting.insert(app_name.to_string()) {
        return Err(format!("Cyclic OS app dependency detected at '{app_name}'"));
    }
    for dependency in os_app_dependencies(app_name) {
        collect_install_order(&dependency, visiting, visited, order)?;
    }
    visiting.remove(app_name);
    visited.insert(app_name.to_string());
    order.push(app_name.to_string());
    Ok(())
}

/// Resolve a deduplicated dependency-first install order for a set of apps.
pub fn resolve_os_app_install_order(app_names: &[String]) -> Result<Vec<String>, String> {
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    let mut order = Vec::new();
    for app_name in app_names {
        collect_install_order(app_name, &mut visiting, &mut visited, &mut order)?;
    }
    Ok(order)
}

fn app_entry(app_name: &str) -> Option<AppEntry> {
    let cat = catalog().read().expect("OS app catalog lock poisoned");
    cat.entries
        .iter()
        .find(|entry| entry.name == app_name)
        .cloned()
}

fn digest_bytes(parts: &[(&str, Vec<u8>)]) -> String {
    let mut hasher = Sha256::new();
    for (name, bytes) in parts {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        hasher.update([0xff]);
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn digest_named_parts(parts: &[(String, Vec<u8>)]) -> String {
    let parts = parts
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.clone()))
        .collect::<Vec<_>>();
    digest_bytes(&parts)
}

fn digest_app_bundle(app_name: &str, bundle: &AppBundle) -> OsAppBundleDigest {
    let entry = app_entry(app_name);
    let app_version = entry
        .as_ref()
        .map(|entry| entry.version.clone())
        .unwrap_or_else(|| "0.1.0".to_string());

    let mut spec_parts = Vec::new();
    for (entity_type, ioa_source) in &bundle.specs {
        spec_parts.push((
            format!("spec:{entity_type}"),
            ioa_source.as_bytes().to_vec(),
        ));
    }
    if let Some(csdl) = &bundle.csdl {
        spec_parts.push(("csdl".to_string(), csdl.as_bytes().to_vec()));
    }
    if let Some(cross_invariants) = &bundle.cross_invariants_toml {
        spec_parts.push((
            "cross-invariants".to_string(),
            cross_invariants.as_bytes().to_vec(),
        ));
    }
    spec_parts.sort_by(|a, b| a.0.cmp(&b.0));

    let mut policy_parts: Vec<(String, Vec<u8>)> = bundle
        .cedar_policies
        .iter()
        .enumerate()
        .map(|(idx, policy)| (format!("policy:{idx}"), policy.as_bytes().to_vec()))
        .collect();
    policy_parts.sort_by(|a, b| a.0.cmp(&b.0));

    let mut wasm_parts = Vec::new();
    for (module_name, wasm_bytes) in &bundle.wasm_modules {
        wasm_parts.push((format!("wasm:{module_name}"), wasm_bytes.clone()));
    }
    for (module_name, config) in &bundle.wasm_module_configs {
        let config_bytes = serde_json::to_vec(config).unwrap_or_default();
        wasm_parts.push((format!("wasm-config:{module_name}"), config_bytes));
    }
    wasm_parts.sort_by(|a, b| a.0.cmp(&b.0));

    let mut content_parts = Vec::new();
    if let Some(app_guide) = entry.as_ref().and_then(|entry| entry.app_guide.as_ref()) {
        content_parts.push(("APP.md".to_string(), app_guide.as_bytes().to_vec()));
    }
    for agent in &bundle.agents {
        content_parts.push((
            format!("agent:{}:content", agent.name),
            agent.content.as_bytes().to_vec(),
        ));
    }
    for skill in &bundle.skills {
        content_parts.push((
            format!(
                "skill:{}:{}",
                skill.agent_name.as_deref().unwrap_or("_system"),
                skill.name
            ),
            skill.content.as_bytes().to_vec(),
        ));
        for companion in &skill.companion_files {
            content_parts.push((
                format!(
                    "skill-companion:{}:{}:{}",
                    skill.agent_name.as_deref().unwrap_or("_system"),
                    skill.name,
                    companion.name
                ),
                companion.content.clone(),
            ));
        }
    }
    for file in &bundle.system_files {
        content_parts.push((
            format!("system:{}", file.relative_path),
            file.content.clone(),
        ));
    }
    for adr in &bundle.adrs {
        content_parts.push((
            format!("adr:{}", adr.file_name),
            adr.content.as_bytes().to_vec(),
        ));
    }
    content_parts.sort_by(|a, b| a.0.cmp(&b.0));

    let mut seed_parts = Vec::new();
    for seed in &bundle.seed_instances {
        seed_parts.push((
            format!(
                "seed:{}:{}",
                seed.entity_type,
                seed.id.as_deref().unwrap_or("_generated")
            ),
            serde_json::to_vec(seed).unwrap_or_default(),
        ));
    }
    seed_parts.sort_by(|a, b| a.0.cmp(&b.0));

    let spec_digest = digest_named_parts(&spec_parts);
    let policy_digest = digest_named_parts(&policy_parts);
    let wasm_digest = digest_named_parts(&wasm_parts);
    let content_digest = digest_named_parts(&content_parts);
    let seed_digest = digest_named_parts(&seed_parts);
    let bundle_digest = digest_bytes(&[
        ("app_name", app_name.as_bytes().to_vec()),
        ("app_version", app_version.as_bytes().to_vec()),
        ("spec", spec_digest.as_bytes().to_vec()),
        ("policy", policy_digest.as_bytes().to_vec()),
        ("wasm", wasm_digest.as_bytes().to_vec()),
        ("content", content_digest.as_bytes().to_vec()),
        ("seed", seed_digest.as_bytes().to_vec()),
    ]);

    OsAppBundleDigest {
        app_name: app_name.to_string(),
        app_version,
        bundle_digest,
        spec_digest,
        policy_digest,
        wasm_digest,
        content_digest,
        seed_digest,
    }
}

/// Compute the current bundle digest for an app in the catalog.
pub fn os_app_bundle_digest(app_name: &str) -> Option<OsAppBundleDigest> {
    get_os_app(app_name).map(|bundle| digest_app_bundle(app_name, &bundle))
}

pub(crate) fn tenant_has_registered_app_specs_for_bundle(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
) -> bool {
    let tenant_id = TenantId::new(tenant);
    let registry = state.registry.read().expect("Spec registry lock poisoned");
    let specs_registered = bundle
        .specs
        .iter()
        .all(|(entity_type, _)| registry.get_table(&tenant_id, entity_type).is_some());
    if !specs_registered {
        return false;
    }

    let Some(csdl_xml) = bundle.csdl.as_deref() else {
        return true;
    };
    let Ok(csdl) = parse_csdl(csdl_xml) else {
        return false;
    };

    for schema in &csdl.schemas {
        for container in &schema.entity_containers {
            for entity_set in &container.entity_sets {
                let expected_type = entity_set
                    .entity_type
                    .rsplit('.')
                    .next()
                    .unwrap_or(&entity_set.entity_type);
                if registry
                    .resolve_entity_type(&tenant_id, &entity_set.name)
                    .as_deref()
                    != Some(expected_type)
                {
                    return false;
                }
            }
        }
    }

    true
}

pub(crate) fn tenant_has_ready_app_specs_for_bundle(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
) -> bool {
    if !tenant_has_registered_app_specs_for_bundle(state, tenant, bundle) {
        return false;
    }

    let tenant_id = TenantId::new(tenant);
    let registry = state.registry.read().expect("Spec registry lock poisoned");
    bundle.specs.iter().all(|(entity_type, _)| {
        matches!(
            registry.get_verification_status(&tenant_id, entity_type),
            Some(VerificationStatus::Completed(_) | VerificationStatus::Restored(_))
        )
    })
}

pub(crate) async fn mark_app_specs_restored_from_matching_digest(
    state: &PlatformState,
    ps: &dyn PlatformStore,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) {
    if bundle.specs.is_empty() {
        return;
    }

    let tenant_id = TenantId::new(tenant);
    let verified_at = temper_runtime::scheduler::sim_now().to_rfc3339();
    let result = EntityVerificationResult {
        all_passed: true,
        levels: vec![EntityLevelSummary {
            level: "BundleDigest".to_string(),
            passed: true,
            summary: format!("Restored from matching OS app bundle digest ({app_name})"),
            details: None,
        }],
        verified_at,
    };

    {
        let mut registry = state.registry.write().expect("Spec registry lock poisoned");
        for (entity_type, _) in &bundle.specs {
            registry.set_verification_status(
                &tenant_id,
                entity_type,
                VerificationStatus::Restored(result.clone()),
            );
        }
    }

    for (entity_type, _) in &bundle.specs {
        if let Err(error) = ps
            .persist_spec_verification(
                tenant,
                entity_type,
                SpecVerificationUpdate {
                    status: "passed",
                    verified: true,
                    levels_passed: Some(1),
                    levels_total: Some(1),
                    verification_result_json: None,
                },
            )
            .await
        {
            tracing::warn!(
                tenant,
                app = %app_name,
                entity_type,
                error = %error,
                "Failed to persist restored OS app spec verification status"
            );
        }
    }
}

async fn record_app_install_metadata(
    state: &PlatformState,
    tenant: &str,
    digest: &OsAppBundleDigest,
    status: &str,
) {
    let Some(ps) = state
        .server
        .event_store
        .as_ref()
        .and_then(|store| store.platform_store())
    else {
        return;
    };

    let record = InstalledAppRecord {
        tenant: tenant.to_string(),
        app_name: digest.app_name.clone(),
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

pub(super) async fn record_app_install_metadata_for_bundle(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) {
    let bundle_digest = digest_app_bundle(app_name, bundle);
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
    let bundle = get_os_app(app_name).ok_or_else(|| format!("OS app '{app_name}' not found"))?;
    let digest = digest_app_bundle(app_name, &bundle);

    if let Some(ps) = state
        .server
        .event_store
        .as_ref()
        .and_then(|store| store.platform_store())
    {
        match ps.get_installed_app(tenant, app_name).await {
            Ok(Some(record)) if record.bundle_digest == digest.bundle_digest => {
                if tenant_has_ready_app_specs_for_bundle(state, tenant, &bundle) {
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

                if tenant_has_registered_app_specs_for_bundle(state, tenant, &bundle) {
                    mark_app_specs_restored_from_matching_digest(
                        state, ps, tenant, app_name, &bundle,
                    )
                    .await;
                    tracing::info!(
                        tenant,
                        app = %app_name,
                        bundle_digest = %digest.bundle_digest,
                        "OS app unchanged; restored spec readiness and skipped hot reconcile"
                    );
                    return Ok(OsAppReconcileResult::Skipped {
                        app_name: app_name.to_string(),
                        bundle_digest: digest.bundle_digest,
                    });
                }
            }
            Ok(Some(_)) | Ok(None) => {}
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

    let install = install_os_app_without_dependencies(state, tenant, app_name).await?;
    Ok(OsAppReconcileResult::Installed {
        app_name: app_name.to_string(),
        bundle_digest: digest.bundle_digest,
        install: Box::new(install),
    })
}
