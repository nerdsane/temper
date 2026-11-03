use std::collections::HashSet;
use std::path::Path;

use sha2::{Digest, Sha256};
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::InstalledAppRecord;

use super::app_catalog::AppCatalog;
use super::{
    AppBundle, AppEntry, AppFileSource, OsAppBundleDigest, OsAppInstallPlan, OsAppReconcileResult,
    catalog, get_os_app, install_os_app_swapping, load_app_bundle, load_tenant_os_app,
    os_app_dependencies, restore_app_specs_from_matching_digest,
    tenant_has_ready_app_specs_for_bundle, tenant_os_app,
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

/// Resolve a deduplicated dependency-first install order for a set of
/// shared-catalog apps.
pub fn resolve_os_app_install_order(app_names: &[String]) -> Result<Vec<String>, String> {
    resolve_os_app_install_order_with_dependencies(app_names, |app_name| {
        os_app_dependencies(None, app_name)
    })
}

/// Resolve a deduplicated dependency-first install order for a set of apps as
/// `tenant` resolves them (its own installs, then the shared catalog).
pub fn resolve_tenant_os_app_install_order(
    tenant: &str,
    app_names: &[String],
) -> Result<Vec<String>, String> {
    resolve_os_app_install_order_with_dependencies(app_names, |app_name| {
        os_app_dependencies(Some(tenant), app_name)
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

fn app_entry(tenant: Option<&str>, app_name: &str) -> Option<AppEntry> {
    let cat = catalog().read().expect("OS app catalog lock poisoned");
    cat.app_entry(tenant, app_name).cloned()
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

pub(super) fn digest_app_bundle(
    tenant: Option<&str>,
    app_name: &str,
    bundle: &AppBundle,
) -> OsAppBundleDigest {
    digest_entry_bundle(app_name, app_entry(tenant, app_name).as_ref(), bundle)
}

/// The bundle digest of each of `apps` of install directory `dir` that
/// loads, read from the files each names and without adding them to the
/// catalog. An app that does not load, such as one with a named file that
/// cannot be read, is left out.
pub fn written_apps_bundle_digests(
    dir: &Path,
    apps: Vec<(String, AppFileSource)>,
) -> Vec<OsAppBundleDigest> {
    let installed = AppCatalog::from_apps(dir.to_path_buf(), apps);
    installed
        .app_entries(None)
        .iter()
        .filter_map(|entry| {
            let bundle = load_app_bundle(installed.app_files(None, &entry.name)?).ok()?;
            Some(digest_entry_bundle(&entry.name, Some(entry), &bundle))
        })
        .collect()
}

fn digest_entry_bundle(
    app_name: &str,
    entry: Option<&AppEntry>,
    bundle: &AppBundle,
) -> OsAppBundleDigest {
    let app_version = entry
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
    if let Some(app_guide) = entry.and_then(|entry| entry.app_guide.as_ref()) {
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

/// Compute the current bundle digest for an app in the shared catalog.
pub fn os_app_bundle_digest(app_name: &str) -> Option<OsAppBundleDigest> {
    get_os_app(app_name).map(|bundle| digest_app_bundle(None, app_name, &bundle))
}

/// Compute the current bundle digest of the app `tenant` resolves for `app_name`.
pub fn tenant_os_app_bundle_digest(tenant: &str, app_name: &str) -> Option<OsAppBundleDigest> {
    tenant_os_app(tenant, app_name).map(|bundle| digest_app_bundle(Some(tenant), app_name, &bundle))
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
    missing_required_wasm_modules(bundle).is_empty()
}

/// The modules `app.toml` marks platform-required or app-required that the
/// bundle has no compiled binary for, sorted.
pub(super) fn missing_required_wasm_modules(bundle: &AppBundle) -> Vec<String> {
    let mut missing: Vec<String> = bundle
        .wasm_module_configs
        .iter()
        .filter(|(module_name, config)| {
            config.is_required() && !bundle.wasm_modules.contains_key(*module_name)
        })
        .map(|(module_name, _)| module_name.clone())
        .collect();
    missing.sort();
    missing
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

/// Whose word an install carries out, which decides the origin it records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallOrigin {
    /// An install request for the files this server holds: a new bundle's
    /// origin becomes `local`; repairing the same bundle retains its source.
    /// A Genesis install writes its own origin after it.
    Request,
    /// A boot replay of a stored install, which restores it as recorded.
    Replay,
}

/// Refuse a boot replay whose bundle is not the version its record names.
///
/// A Genesis record names a registry version by its bundle digest. When that
/// version's files are gone and cannot be fetched, the app name can still
/// resolve to another bundle, such as a shared app of the same name. A replay
/// then installs nothing: the app is unavailable until a later boot finds the
/// recorded version, and the record stays as it is. A local record's version
/// is whatever local files the server holds, so any bundle matches it.
pub(super) async fn ensure_replay_bundle_is_recorded(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) -> Result<(), String> {
    let Some(ps) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return Ok(());
    };
    let stored = ps
        .get_installed_app(tenant, app_name)
        .await
        .map_err(|error| {
            format!(
                "os-app '{app_name}' for tenant '{tenant}' is unavailable: its install record \
             cannot be read ({error}), so its version cannot be checked; nothing was installed"
            )
        })?;
    let Some(stored) = stored.filter(|stored| stored.source_kind != "local") else {
        return Ok(());
    };
    let resolved = digest_app_bundle(Some(tenant), app_name, bundle);
    if resolved.bundle_digest == stored.bundle_digest {
        return Ok(());
    }
    Err(format!(
        "os-app '{app_name}' for tenant '{tenant}' is unavailable: its {} install '{}' \
         records bundle {}, but the only bundle of that name here is {}; nothing was \
         installed, and the record is kept for the next boot",
        stored.source_kind, stored.app_ref, stored.bundle_digest, resolved.bundle_digest
    ))
}

async fn record_app_install_metadata(
    state: &PlatformState,
    tenant: &str,
    digest: &OsAppBundleDigest,
    status: &str,
    origin: InstallOrigin,
) {
    let Some(ps) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return;
    };

    let stored = match ps.get_installed_app(tenant, &digest.app_name).await {
        Ok(stored) => stored,
        Err(error) => {
            tracing::warn!(
                tenant,
                app = %digest.app_name,
                error = %error,
                "Failed to read the stored install record; leaving its provenance as it is"
            );
            return;
        }
    };
    // A replay never changes the origin. A request repairing the same bundle
    // also retains its valid source: a pushed root may reconcile catalog
    // dependencies whose provenance it does not rewrite at commit. Changed
    // bundles become local until their source records the new provenance.
    if origin == InstallOrigin::Replay
        && stored
            .as_ref()
            .is_some_and(|record| record.source_kind != "local")
    {
        return;
    }
    let retained = stored.filter(|record| {
        record.source_kind != "local" && record.bundle_digest == digest.bundle_digest
    });
    let record = retained
        .map(|record| InstalledAppRecord {
            status: status.to_string(),
            ..record
        })
        .unwrap_or_else(|| InstalledAppRecord {
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
        });

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
    origin: InstallOrigin,
) {
    let bundle_digest = digest_app_bundle(Some(tenant), app_name, bundle);
    record_app_install_metadata(state, tenant, &bundle_digest, "installed", origin).await;
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
    let result =
        reconcile_os_app_swapping(state, tenant, app_name, super::Swap::ModulesFirst).await?;
    if let OsAppReconcileResult::Installed { install, .. } = &result {
        install.publish_reviews();
    }
    Ok(result)
}

/// [`reconcile_os_app`] with the order its modules and specs go in. The closure
/// caller publishes reviews after its readiness checks and provenance writes.
pub(crate) async fn reconcile_os_app_swapping(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    swap: super::Swap,
) -> Result<OsAppReconcileResult, String> {
    let bundle = load_tenant_os_app(tenant, app_name)?;
    let digest = digest_app_bundle(Some(tenant), app_name, &bundle);

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
                let install = install_os_app_swapping(
                    state,
                    tenant,
                    app_name,
                    plan,
                    InstallOrigin::Request,
                    swap,
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

    let install = install_os_app_swapping(
        state,
        tenant,
        app_name,
        OsAppInstallPlan::all(),
        InstallOrigin::Request,
        swap,
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
