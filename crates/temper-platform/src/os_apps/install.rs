//! OS app installation pipeline (durable persistence, verification cascade,
//! Cedar policies, WASM modules, and post-install bootstrap).

use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{emit_csdl_xml, merge_csdl, parse_csdl};

use super::bootstrap::{
    bootstrap_adrs, bootstrap_app_entity, bootstrap_seed_data, bootstrap_skills,
};
use super::bundle::load_app_bundle;
use super::{
    AppBundle, InstallResult, OsAppInstallPlan, WasmModuleCriticality, WasmStartupLoading,
    agent_bootstrap, catalog, reconcile, resolve_os_app_install_order, system_files,
};
use crate::bootstrap;
use crate::state::PlatformState;

/// Install an OS app into a tenant (workspace).
///
/// Reads app files from disk, runs the verification cascade, registers
/// specs in the SpecRegistry, loads Cedar policies, and **persists
/// everything to the platform DB** so specs survive redeployments.
///
/// **Write ordering:** Turso first, then memory. If Turso persistence fails
/// the operation returns an error *before* touching in-memory state, so the
/// registry and Cedar engine stay consistent with the durable store.
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

pub(super) async fn install_os_app_with_plan(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    plan: OsAppInstallPlan,
) -> Result<InstallResult, String> {
    let app_dir = {
        let cat = catalog().read().unwrap(); // ci-ok: infallible lock
        cat.paths.get(app_name).cloned().ok_or_else(|| {
            let known: Vec<String> = cat.paths.keys().cloned().collect();
            format!("OS app '{app_name}' not found in catalog (known path keys: {known:?})")
        })?
    };
    let bundle = load_app_bundle(&app_dir).ok_or_else(|| {
        format!(
            "OS app '{app_name}' is registered at '{}' but its bundle failed to load",
            app_dir.display()
        )
    })?;
    let tenant_id = TenantId::new(tenant);
    let replace_uploaded_wasm = if plan.wasm && !bundle.wasm_modules.is_empty() {
        should_replace_uploaded_wasm_for_bundle_reconcile(state, tenant, app_name, &bundle).await
    } else {
        false
    };

    if bundle.adrs.is_empty() {
        tracing::warn!(
            tenant,
            app = %app_name,
            "App installed with no ADRs; add adrs/*.md to record design decisions"
        );
    }

    // Classify each bundle spec as added / updated / skipped, and compute the
    // merged CSDL only when the reconcile plan needs spec work.
    let (mut added, mut updated, mut skipped, merged_csdl) = if plan.specs {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        let mut added = Vec::new();
        let mut updated = Vec::new();
        let mut skipped = Vec::new();
        for (entity_type, ioa_source) in &bundle.specs {
            let incoming_hash = temper_store_turso::spec_content_hash(ioa_source);
            match registry.get_spec(&tenant_id, entity_type) {
                Some(existing) => {
                    let existing_hash = temper_store_turso::spec_content_hash(&existing.ioa_source);
                    let verified = registry
                        .get_verification_status(&tenant_id, entity_type)
                        .map(|s| s.is_passed())
                        .unwrap_or(false);
                    if incoming_hash == existing_hash && verified {
                        skipped.push(entity_type.to_string());
                    } else {
                        updated.push(entity_type.to_string());
                    }
                }
                None => {
                    added.push(entity_type.to_string());
                }
            }
        }
        // App installs must preserve existing tenant types.
        let merged_csdl = if let Some(ref csdl) = bundle.csdl {
            if let Some(existing) = registry.get_tenant(&tenant_id) {
                let incoming = parse_csdl(csdl)
                    .map_err(|e| format!("Failed to parse CSDL for os-app '{app_name}': {e}"))?;
                Some(emit_csdl_xml(&merge_csdl(&existing.csdl, &incoming)))
            } else {
                Some(csdl.clone())
            }
        } else {
            // No CSDL in bundle; keep existing if any.
            registry
                .get_tenant(&tenant_id)
                .map(|t| emit_csdl_xml(&t.csdl))
        };
        (added, updated, skipped, merged_csdl)
    } else {
        (Vec::new(), Vec::new(), Vec::new(), None)
    };
    // Sort for deterministic output.
    added.sort();
    updated.sort();
    skipped.sort();

    // Build the full Cedar policy text for this tenant (existing + new).
    let combined_policy = if plan.policies && !bundle.cedar_policies.is_empty() {
        let combined: String = bundle.cedar_policies.join("\n");
        let policies = state.server.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        let existing = policies.get(tenant).cloned().unwrap_or_default();
        let full_text = if existing.is_empty() {
            combined
        } else {
            format!("{existing}\n{combined}")
        };
        Some(full_text)
    } else {
        None
    };

    // ── Step 1: Persist to Turso FIRST (if available). ──────────────
    // If any write fails, bail before touching in-memory state.
    if let Some(turso) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.turso.as_ref())
        .and_then(|provider| provider.platform_store())
    {
        if plan.specs
            && let Some(ref merged) = merged_csdl
        {
            // Collect only this app's specs with their content hashes for a
            // single transactional write. Existing tenant specs are preserved by
            // the merged CSDL, but no longer re-written on every app install.
            let owned: Vec<(String, String, String, String)> = bundle
                .specs
                .iter()
                .map(|(et, ioa)| {
                    let hash = temper_store_turso::spec_content_hash(ioa);
                    (et.clone(), ioa.clone(), merged.clone(), hash)
                })
                .collect();
            let refs: Vec<(&str, &str, &str, &str)> = owned
                .iter()
                .map(|(et, ioa, csdl, h)| (et.as_str(), ioa.as_str(), csdl.as_str(), h.as_str()))
                .collect();
            turso
                .upsert_specs_and_commit(tenant, &refs, combined_policy.as_deref(), app_name)
                .await
                .map_err(|e| format!("Failed to persist and commit specs: {e}"))?;
        } else if let Some(ref policy_text) = combined_policy {
            turso
                .upsert_tenant_policy(tenant, policy_text)
                .await
                .map_err(|e| format!("Failed to persist Cedar policy: {e}"))?;
            turso
                .record_installed_app(tenant, app_name)
                .await
                .map_err(|e| format!("Failed to record os-app installation: {e}"))?;
        }
        if plan.specs
            && let Some(ref cross_invariants_toml) = bundle.cross_invariants_toml
        {
            turso
                .upsert_tenant_constraints(tenant, cross_invariants_toml)
                .await
                .map_err(|e| format!("Failed to persist cross-invariants: {e}"))?;
        }
    } else if let Some(ps) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    {
        if plan.specs
            && let Some(ref merged) = merged_csdl
        {
            for (entity_type, ioa_source) in &bundle.specs {
                let hash = temper_store_turso::spec_content_hash(ioa_source);
                ps.upsert_spec(tenant, entity_type, ioa_source, merged, &hash)
                    .await
                    .map_err(|e| format!("Failed to persist spec {entity_type}: {e}"))?;
            }
        }
        if let Some(ref policy_text) = combined_policy {
            ps.upsert_tenant_policy(tenant, policy_text)
                .await
                .map_err(|e| format!("Failed to persist Cedar policy: {e}"))?;
        }
        if plan.specs
            && let Some(ref cross_invariants_toml) = bundle.cross_invariants_toml
        {
            ps.upsert_tenant_constraints(tenant, cross_invariants_toml)
                .await
                .map_err(|e| format!("Failed to persist cross-invariants: {e}"))?;
        }
        ps.record_installed_app(tenant, app_name)
            .await
            .map_err(|e| format!("Failed to record os-app installation: {e}"))?;
        if plan.specs {
            // Commit only when this path used individual spec writes.
            ps.commit_specs(tenant)
                .await
                .map_err(|e| format!("Failed to commit specs: {e}"))?;
        }
    }

    // ── Step 2: Bootstrap into memory (verification + registry). ────
    // Only process specs whose content has changed (added or updated);
    // skipped specs are already loaded with identical content.
    if plan.specs && !bundle.specs.is_empty() {
        let specs_to_bootstrap: Vec<(&str, &str)> = bundle
            .specs
            .iter()
            .map(|(et, src)| (et.as_str(), src.as_str()))
            .collect();
        let verified_cache = if let Some(turso) = state
            .server
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.turso.as_ref())
            .and_then(|provider| provider.platform_store())
        {
            turso
                .load_verification_cache(tenant)
                .await
                .unwrap_or_default()
        } else if let Some(ps) = state
            .server
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.platform.clone())
        {
            ps.load_verification_cache(tenant).await.unwrap_or_default()
        } else {
            std::collections::BTreeMap::new()
        };

        if let Some(ref merged) = merged_csdl {
            // Even when every spec is byte-for-byte unchanged, we still need to
            // merge the app's CSDL back into the in-memory registry so entity-set
            // mappings survive partial restores and process restarts. Re-running
            // bootstrap also lets recovery heal specs that were durably left
            // in `pending` by older installs.
            let spec_hashes = bootstrap::bootstrap_tenant_specs(
                state,
                tenant,
                merged,
                &specs_to_bootstrap,
                bootstrap::BootstrapTenantSpecsOptions {
                    merge: true,
                    label: &format!("OsApp({app_name})"),
                    verified_cache: &verified_cache,
                    cross_invariants_source: bundle.cross_invariants_toml.as_deref(),
                    verification_mode: bootstrap::BootstrapSpecVerificationMode::TrustBundle,
                },
            );

            if let Some(ps) = state
                .server
                .storage_stack
                .as_ref()
                .and_then(|stack| stack.platform.clone())
            {
                bootstrap::persist_bootstrap_verification(
                    ps.as_ref(),
                    tenant,
                    &specs_to_bootstrap,
                    merged,
                    &spec_hashes,
                    &verified_cache,
                )
                .await;
            }
        }
        // App installs can introduce or update inline entity triggers.
        // Refresh the live dispatcher after the registry mutation so the
        // newly bootstrapped trigger graph is active for subsequent traffic.
        state.server.rebuild_reaction_dispatcher();
    }

    // App installs can add or change cross-entity reactions. Refresh the live
    // dispatcher immediately so the newly registered tenant config takes effect
    // without requiring a process restart or a separate specs reload.
    if plan.specs {
        state.server.rebuild_reaction_dispatcher();
    }

    // ── Step 3: Load Cedar policies into memory. ────────────────────
    if let Some(ref policy_text) = combined_policy {
        if let Err(e) = state
            .server
            .authz
            .reload_tenant_policies(tenant, policy_text)
        {
            tracing::warn!(
                tenant,
                error = %e,
                "Failed to reload tenant Cedar policies after os-app install"
            );
        } else {
            let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
            policies.insert(tenant.to_string(), policy_text.clone());
        }
    }

    // ── Step 4: Persist/register WASM modules, warming only eager modules. ──
    //
    // Source-aware preservation: if a (tenant, module) row in the durable store
    // has source='upload' (i.e., a hot upload from `POST /api/wasm/modules/{name}`),
    // preserve it across same-bundle restarts. When the installed app metadata
    // says the bundled WASM digest changed, the bundle is a newer deployment and
    // must replace stale uploads so production executes the shipped module.
    let mut wasm_registered = Vec::new();
    let mut wasm_skipped = Vec::new();
    let mut wasm_failures = Vec::new();
    if plan.wasm {
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

        for (module_name, wasm_bytes) in &bundle.wasm_modules {
            let module_config = bundle.wasm_module_configs.get(module_name);
            let hash = temper_wasm::WasmEngine::hash_module(wasm_bytes);

            if let Some((existing_hash, existing_source)) = existing_sources.get(module_name)
                && existing_source == "upload"
                && existing_hash != &hash
            {
                if replace_uploaded_wasm {
                    tracing::info!(
                        tenant,
                        module = %module_name,
                        bundled_hash = %hash,
                        upload_hash = %existing_hash,
                        "Replacing hot-uploaded WASM module: bundled app WASM digest changed"
                    );
                } else {
                    tracing::info!(
                        tenant,
                        module = %module_name,
                        bundled_hash = %hash,
                        upload_hash = %existing_hash,
                        "Skipping bundled install: hot-uploaded module preserved"
                    );
                    wasm_skipped.push(module_name.clone());
                    continue;
                }
            }

            let upsert_result = if replace_uploaded_wasm {
                state
                    .server
                    .upsert_bundled_wasm_module_replacing_upload(
                        tenant,
                        module_name,
                        wasm_bytes,
                        &hash,
                    )
                    .await
            } else {
                state
                    .server
                    .upsert_wasm_module(tenant, module_name, wasm_bytes, &hash, "bundled")
                    .await
            };
            if let Err(e) = upsert_result {
                tracing::warn!(
                    tenant,
                    module = %module_name,
                    error = %e,
                    "Failed to persist WASM module to durable store (continuing in-memory only)"
                );
            }
            {
                let mut wasm_reg = state.server.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                wasm_reg.register(&tenant_id, module_name, &hash);
            }

            if matches!(
                module_config.map(|config| config.startup_loading),
                Some(WasmStartupLoading::Eager)
            ) && let Err(e) = state.server.wasm_engine.compile_and_cache(wasm_bytes)
            {
                wasm_failures.push(module_name.clone());
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
                startup_loading = ?module_config
                    .map(|config| config.startup_loading)
                    .unwrap_or_default(),
                "WASM module registered from OS app"
            );
            wasm_registered.push(module_name.clone());
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
    }

    tracing::info!(
        "Installed os-app '{app_name}' for tenant '{tenant}': \
         added={:?} updated={:?} skipped={:?} wasm={:?}",
        added,
        updated,
        skipped,
        wasm_registered,
    );

    // ── Step 5: Bootstrap App entity + APP.md. ──────────────────────────
    let (agents_bootstrapped, skills_bootstrapped, adrs_bootstrapped) = if plan.content {
        let app_id = bootstrap_app_entity(state, &tenant_id, tenant, app_name).await;

        // ── Step 6: Bootstrap agents (returns name→uuid map). ──────────
        let (agents_bootstrapped, agent_uuid_map) = agent_bootstrap::bootstrap_agents(
            state,
            &tenant_id,
            tenant,
            &bundle.agents,
            app_id.as_deref(),
        )
        .await;

        // ── Step 7: Bootstrap skills (agent-scoped + system). ──────────
        let skills_bootstrapped =
            bootstrap_skills(state, &tenant_id, tenant, &bundle.skills, &agent_uuid_map).await;

        // ── Step 7b: Bootstrap system files (e.g. mode-instructions). ─
        system_files::bootstrap_system_files(state, &tenant_id, tenant, &bundle.system_files).await;

        // ── Step 8: Bootstrap ADRs into TemperFS. ─────────────────────
        let adrs_bootstrapped =
            bootstrap_adrs(state, &tenant_id, tenant, app_name, &bundle.adrs).await;

        (agents_bootstrapped, skills_bootstrapped, adrs_bootstrapped)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    // ── Step 9: Create seed instances. ───────────────────────────────
    let seed_created = if plan.seed {
        bootstrap_seed_data(state, &tenant_id, tenant, &bundle.seed_instances).await
    } else {
        Vec::new()
    };

    reconcile::record_app_install_metadata_for_bundle(state, tenant, app_name, &bundle).await;

    Ok(InstallResult {
        added,
        updated,
        skipped,
        wasm_modules: wasm_registered,
        wasm_skipped,
        wasm_failures,
        agents: agents_bootstrapped,
        skills: skills_bootstrapped,
        adrs_bootstrapped,
        seed_instances: seed_created,
    })
}

async fn should_replace_uploaded_wasm_for_bundle_reconcile(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) -> bool {
    let Some(ps) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return false;
    };
    let digest = reconcile::digest_app_bundle(app_name, bundle);
    match ps.get_installed_app(tenant, app_name).await {
        Ok(Some(record)) => record.wasm_digest != digest.wasm_digest,
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(
                tenant,
                app = %app_name,
                error = %error,
                "Failed to read OS app metadata while deciding WASM hot-upload replacement"
            );
            false
        }
    }
}

/// Backward-compatible alias.
pub async fn install_skill(
    state: &PlatformState,
    tenant: &str,
    skill_name: &str,
) -> Result<InstallResult, String> {
    install_os_app(state, tenant, skill_name).await
}
