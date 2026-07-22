use std::collections::HashSet;

use sha2::{Digest, Sha256};
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::InstalledAppRecord;

use super::{
    AppBundle, AppEntry, OsAppBundleDigest, OsAppInstallPlan, OsAppReconcileResult, catalog,
    get_os_app, install_os_app_with_plan_under_guard, os_app_dependencies,
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

pub(super) fn digest_app_bundle(app_name: &str, bundle: &AppBundle) -> OsAppBundleDigest {
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
        .cedar_policy_sources
        .iter()
        .map(|source| {
            (
                format!("policy:{}", source.relative_path),
                source.text.as_bytes().to_vec(),
            )
        })
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

pub(super) fn plan_reconcile_from_installed_record(
    record: &InstalledAppRecord,
    digest: &OsAppBundleDigest,
    specs_ready: bool,
    wasm_registered: bool,
    policies_active: bool,
) -> OsAppInstallPlan {
    // The atomic publication transaction records target digests before the
    // runtime/WASM/content/seed phases run. A non-installed record therefore
    // proves only intent, not completion of any post-commit phase. Retry the
    // complete idempotent bundle workflow instead of letting target digests
    // suppress work that may never have started.
    if record.status != "installed" {
        return OsAppInstallPlan::all();
    }
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

/// Reconcile one app without recursively processing dependencies.
///
/// Callers that need dependencies should first call
/// [`resolve_os_app_install_order`] and reconcile each returned app once.
pub async fn reconcile_os_app(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<OsAppReconcileResult, String> {
    reconcile_os_app_after_generation(state, tenant, app_name, None).await
}

/// Reconcile one app after a request-to-publication handoff. When supplied,
/// `expected_generation` rejects an intervening complete tenant generation
/// before any candidate state is read or made durable.
pub(crate) async fn reconcile_os_app_after_generation(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    expected_generation: Option<u64>,
) -> Result<OsAppReconcileResult, String> {
    let tenant_id = TenantId::new(tenant);
    let mut publication_guard = match expected_generation {
        Some(expected_generation) => {
            state
                .server
                .begin_spec_publication_after_drain(&tenant_id, expected_generation)
                .await?
        }
        None => state.server.begin_spec_publication(&tenant_id).await?,
    };
    reconcile_os_app_under_guard(state, tenant, app_name, None, &mut publication_guard).await
}

pub(crate) async fn reconcile_os_app_under_guard(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    publication_record_override: Option<InstalledAppRecord>,
    publication_guard: &mut temper_server::state::SpecPublicationGuard,
) -> Result<OsAppReconcileResult, String> {
    let bundle = get_os_app(app_name).ok_or_else(|| format!("OS app '{app_name}' not found"))?;
    let digest = digest_app_bundle(app_name, &bundle);

    if let Some(ps) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    {
        match ps.get_installed_app(tenant, app_name).await {
            Ok(Some(record)) => {
                let specs_ready = tenant_has_ready_app_specs_for_bundle(state, tenant, &bundle);
                let wasm_registered = tenant_has_registered_wasm_for_bundle(state, tenant, &bundle);
                let durable_wasm_ready =
                    tenant_has_durable_wasm_for_bundle(state, tenant, &bundle).await;
                let wasm_ready = wasm_registered && durable_wasm_ready;
                let policies_active = tenant_has_active_policies_for_bundle(state, tenant, &bundle);

                if record.status == "installed"
                    && record.bundle_digest == digest.bundle_digest
                    && specs_ready
                    && wasm_ready
                    && policies_active
                    && publication_record_override
                        .as_ref()
                        .is_none_or(|desired| installed_record_provenance_matches(&record, desired))
                    && !state.server.spec_publication_gated(&TenantId::new(tenant))
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
                let install = install_os_app_with_plan_under_guard(
                    state,
                    tenant,
                    app_name,
                    plan,
                    publication_record_override,
                    publication_guard,
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

    let install = install_os_app_with_plan_under_guard(
        state,
        tenant,
        app_name,
        OsAppInstallPlan::all(),
        publication_record_override,
        publication_guard,
    )
    .await?;
    Ok(OsAppReconcileResult::Installed {
        app_name: app_name.to_string(),
        bundle_digest: digest.bundle_digest,
        install: Box::new(install),
    })
}

fn installed_record_provenance_matches(
    existing: &InstalledAppRecord,
    desired: &InstalledAppRecord,
) -> bool {
    existing.source_kind == desired.source_kind
        && existing.app_ref == desired.app_ref
        && existing.version_hash == desired.version_hash
        && existing.pinned_version_hash == desired.pinned_version_hash
        && existing.current_version_hash == desired.current_version_hash
        && existing.follow_policy == desired.follow_policy
        && existing.closure_id == desired.closure_id
        && existing.registry_url == desired.registry_url
        && existing.registry_tenant == desired.registry_tenant
}

/// ARN-68: after a reconcile registers changed specs, re-key any type whose declared
/// key-set changed (a newly-declared `[[key]]` on an already-installed type, e.g.
/// Directory `ws_path` added after `name_parent`). The once-per-boot startup key-index
/// backfill can fire BEFORE this (late — post-boot in prod) reconcile registers the key,
/// see only the old key-set, and skip — so the added key would never backfill for
/// existing entities and reads scan → 413. Running the key-set-aware re-key here, after
/// registration, closes that repair gap. Contract activation, exact entity replay,
/// watermark publication, and empty-contract row release happen synchronously around
/// the table swap. This detached pass maintains vector projections and idempotently
/// rechecks key coverage; unchanged ready types reuse their fenced proof without
/// replay. See ADR-0153 and ADR-0192.
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
