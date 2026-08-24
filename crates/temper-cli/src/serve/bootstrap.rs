//! Startup phase helpers for `temper serve`.
//!
//! Each function represents an explicit phase of the startup pipeline.
//! The `run` coordinator in `mod.rs` calls these in sequence.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::authz::load_and_activate_tenant_policies;
use temper_server::registry::SpecRegistry;
use temper_server::registry_bootstrap::{
    restore_registry_from_postgres, restore_registry_from_turso,
};
use temper_server::storage::StorageStack;
use temper_server::webhooks::WebhookDispatcher;
use temper_store_redis::RedisEventStore;
use temper_store_turso::{TenantStoreRouter, TursoEventStore};

use crate::StorageBackend;

use super::loader::load_into_registry;
use super::storage::{
    connect_postgres_store, redact_connection_url, upsert_loaded_specs_to_postgres,
    upsert_loaded_specs_to_turso,
};

pub(super) fn resolve_data_dir(override_path: Option<PathBuf>) -> Result<PathBuf> {
    let data_dir = override_path.unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        Path::new(&home).join(".local/share/temper")
    });
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("Failed to create data directory {}", data_dir.display()))?;
    Ok(data_dir)
}

pub(super) fn seed_cedar_policies(
    state: &PlatformState,
    tenant_policy_seed: BTreeMap<String, String>,
) {
    for (tenant, policy_text) in &tenant_policy_seed {
        if let Err(error) = state
            .server
            .authz
            .reload_tenant_policies(tenant, policy_text)
        {
            eprintln!("  Warning: failed to load Cedar policies for tenant '{tenant}': {error}");
            continue;
        }
    }
    let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
    for (tenant, policy_text) in tenant_policy_seed {
        policies.insert(tenant, policy_text);
    }
}

/// Phase 1: Initialize the storage backend (Postgres, Turso, Redis, or memory).
pub(super) async fn init_storage(
    storage: StorageBackend,
    storage_explicit: bool,
    data_dir: Option<&Path>,
) -> Result<(Option<sqlx::PgPool>, Option<StorageStack>)> {
    let mut pg_pool: Option<sqlx::PgPool> = None;
    let storage_stack: Option<StorageStack> = match storage {
        StorageBackend::Postgres => {
            if let Ok(database_url) = std::env::var("DATABASE_URL") {
                let (store, pool) = connect_postgres_store(&database_url).await?;
                println!(
                    "  Storage: postgres ({})",
                    redact_connection_url(&database_url)
                );
                pg_pool = Some(pool);
                Some(store)
            } else if storage_explicit {
                anyhow::bail!("DATABASE_URL is required when --storage postgres is selected");
            } else {
                println!("  Storage: memory (in-memory only)");
                println!("  No DATABASE_URL — running in-memory only (state not persisted).");
                None
            }
        }
        StorageBackend::Turso => {
            // Multi-tenant cloud mode: TURSO_PLATFORM_URL points at the shared platform DB.
            if data_dir.is_none()
                && let Ok(platform_url) = std::env::var("TURSO_PLATFORM_URL")
            {
                let platform_token = std::env::var("TURSO_PLATFORM_AUTH_TOKEN").ok();
                let local_base_dir = std::env::var("TURSO_LOCAL_BASE_DIR").ok().or_else(|| {
                    let home = std::env::var("HOME").ok()?;
                    Some(format!("{home}/.local/share/temper/tenants"))
                });

                let mut router = TenantStoreRouter::new(
                    &platform_url,
                    platform_token.as_deref(),
                    local_base_dir,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Failed to create tenant store router: {e}"))?;

                // Wire Turso Cloud provisioning when API credentials are available.
                if let (Ok(api_token), Ok(org)) =
                    (std::env::var("TURSO_API_TOKEN"), std::env::var("TURSO_ORG"))
                {
                    let group = std::env::var("TURSO_GROUP").ok();
                    router = router.with_cloud_config(api_token, org, group);
                    println!("  Cloud provisioning: enabled");
                }

                let tenant_count = router.connected_tenants().await.len();
                println!(
                    "  Storage: turso-routed ({platform_url}, {tenant_count} tenants connected)"
                );
                Some(StorageStack::from_tenant_router(router))
            } else {
                // Single-DB mode (local dev).
                let turso_url = match data_dir {
                    Some(data_dir) => {
                        let db_path = data_dir.join("agents.db");
                        let parent_dir = db_path.parent().context(
                            "Failed to determine parent directory for default Turso DB path",
                        )?;
                        fs::create_dir_all(parent_dir).with_context(|| {
                            format!(
                                "Failed to create default Turso DB directory: {}",
                                parent_dir.display()
                            )
                        })?;
                        format!("file:{}", db_path.display())
                    }
                    None => match std::env::var("TURSO_URL") {
                        Ok(url) => url,
                        Err(_) => {
                            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                            let db_path = Path::new(&home).join(".local/share/temper/agents.db");
                            let parent_dir = db_path.parent().context(
                                "Failed to determine parent directory for default Turso DB path",
                            )?;
                            fs::create_dir_all(parent_dir).with_context(|| {
                                format!(
                                    "Failed to create default Turso DB directory: {}",
                                    parent_dir.display()
                                )
                            })?;
                            format!("file:{}", db_path.display())
                        }
                    },
                };
                let turso_token = if data_dir.is_some() {
                    None
                } else {
                    std::env::var("TURSO_AUTH_TOKEN").ok()
                };
                let store = TursoEventStore::new(&turso_url, turso_token.as_deref())
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to connect to Turso/libSQL: {e}"))?;
                println!("  Storage: turso ({})", turso_url);
                Some(StorageStack::from_turso(store))
            }
        }
        StorageBackend::Redis => {
            let redis_url = std::env::var("REDIS_URL")
                .context("REDIS_URL is required when --storage redis is selected")?;
            let store = RedisEventStore::new(&redis_url)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to connect to Redis: {e}"))?;
            println!("  Storage: redis ({})", redact_connection_url(&redis_url));
            Some(StorageStack::from_redis(store))
        }
    };
    Ok((pg_pool, storage_stack))
}

/// Phase 2: Build the spec registry from persistent storage and disk apps.
pub(super) async fn build_registry(
    pg_pool: Option<&sqlx::PgPool>,
    storage_stack: Option<&StorageStack>,
    apps: &[(String, String)],
) -> Result<(SpecRegistry, BTreeMap<String, String>)> {
    let mut registry = SpecRegistry::new();
    let mut tenant_policy_seed = BTreeMap::new();

    // Restore from persistent backend first.
    if let Some(pool) = pg_pool {
        let restored = restore_registry_from_postgres(&mut registry, pool)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        if restored > 0 {
            println!("  Restored {restored} specs from Postgres.");
        }
    } else if let Some(provider) = storage_stack.and_then(|stack| stack.turso.clone()) {
        let mut total_restored = 0usize;
        if let Some(platform_store) = provider.platform_store() {
            let restored = restore_registry_from_turso(&mut registry, &platform_store)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            total_restored += restored;
        }
        for tenant_id in provider.connected_tenants().await {
            if let Some(store) = provider.store_for_tenant(&tenant_id).await {
                let restored = restore_registry_from_turso(&mut registry, &store)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                total_restored += restored;
            }
        }
        if total_restored > 0 {
            println!("  Restored {total_restored} specs from Turso.");
        }
    }

    // Load app specs from disk, persisting to backend.
    for (tenant, specs_dir) in apps {
        println!("  Loading app: {tenant} from {specs_dir}");
        let loaded = load_into_registry(&mut registry, specs_dir, tenant)?;
        if let Some(text) = loaded.cedar_policy_text.as_ref() {
            tenant_policy_seed.insert(tenant.clone(), text.clone());
        }
        if let Some(pool) = pg_pool {
            upsert_loaded_specs_to_postgres(pool, tenant, &loaded).await?;
        } else if let Some(provider) = storage_stack.and_then(|stack| stack.turso.clone())
            && let Some(store) = provider.store_for_tenant(tenant).await
        {
            upsert_loaded_specs_to_turso(&store, tenant, &loaded).await?;
        }
    }

    Ok((registry, tenant_policy_seed))
}

/// Phase 4: Load webhook configurations from app directories.
pub(super) fn load_webhooks(apps: &[(String, String)]) -> Option<Arc<WebhookDispatcher>> {
    let mut all_configs = Vec::new();
    for (tenant, specs_dir) in apps {
        let path = Path::new(specs_dir).join("webhooks.toml");
        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(source) => match WebhookDispatcher::from_toml(&source) {
                    Ok(d) => {
                        println!("  Loaded webhooks.toml for {tenant}");
                        all_configs.extend(d.configs().iter().cloned());
                    }
                    Err(e) => {
                        eprintln!("  Warning: failed to parse webhooks.toml for {tenant}: {e}")
                    }
                },
                Err(e) => {
                    eprintln!("  Warning: failed to read webhooks.toml for {tenant}: {e}")
                }
            }
        }
    }
    if all_configs.is_empty() {
        None
    } else {
        Some(Arc::new(WebhookDispatcher::new(all_configs)))
    }
}

/// Phase 5: Hydrate entities from the event store for each tenant.
pub(super) async fn hydrate_entities(state: &PlatformState, apps: &[(String, String)]) {
    if state.server.storage_stack.is_none() {
        return;
    }
    let eager_hydrate = std::env::var("TEMPER_EAGER_HYDRATE") // determinism-ok: read once at startup
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        })
        .unwrap_or(false);
    let mut all_tenants = Vec::new();
    for (tenant, _dir) in apps {
        let tenant_id = TenantId::new(tenant.as_str());
        if eager_hydrate {
            state.server.hydrate_from_store(&tenant_id).await;
        } else {
            state.server.populate_index_from_store(&tenant_id).await;
        }
        all_tenants.push(tenant_id);
    }
    // In TenantRouted mode, also hydrate all registered tenants.
    if let Some(provider) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.turso.clone())
    {
        for tenant in provider.connected_tenants().await {
            let tenant_id = TenantId::new(&tenant);
            if eager_hydrate {
                state.server.hydrate_from_store(&tenant_id).await;
            } else {
                state.server.populate_index_from_store(&tenant_id).await;
            }
            all_tenants.push(tenant_id);
        }
    }

    // Background task: backfill the declared-key index, then the broad field index,
    // from snapshots — after the entity index is populated so pre-existing entities
    // are covered.
    let server = state.server.clone();
    tokio::spawn(async move {
        // ADR-0153: the cheap declared-key backfill first (K = 1-3 rows per entity).
        // It keys pre-existing entities and sets the per-(tenant,type) watermark, so
        // their point reads resolve present/absent in O(log n) instead of the
        // full-type scan that 413s at tenant scale — independent of the heavy
        // field-index re-scan. No-op on backends that don't co-commit keys (those
        // never become authoritative, so a keyed miss stays scan-safe).
        for tenant_id in &all_tenants {
            server.populate_key_index_from_snapshots(tenant_id).await;
        }
        // ADR-0155: backfill the declared-vector index (parse + upsert one row per
        // declared path per entity), so pre-existing / write-behind entities are
        // rankable by Temper.Nearest and the per-type watermark is set.
        for tenant_id in &all_tenants {
            server.populate_vector_index_from_snapshots(tenant_id).await;
        }
        // Then the broad field index for OData filter push-down.
        for tenant_id in all_tenants {
            server.populate_field_index_from_snapshots(&tenant_id).await;
        }
    });
}

/// Phase 6: Recover Cedar policies from persistent storage.
///
/// Two-pass recovery:
/// 1. Legacy pass: reads from `tenant_policies` (flat blob per tenant) for
///    backward compatibility with data written before this migration.
/// 2. New pass: reads from `policies` (per-entry rows with hash tracking) via
///    [`load_and_activate_tenant_policies`].  The new table takes precedence for
///    any tenant that has entries there, overwriting what the legacy pass loaded.
pub(super) async fn recover_cedar_policies(state: &PlatformState) {
    let Some(stack) = state.server.storage_stack.as_ref() else {
        return;
    };

    let mut all_policy_rows: Vec<(String, String)> = Vec::new();

    if let Some(platform_store) = stack.platform.as_ref() {
        match platform_store.load_tenant_policies().await {
            Ok(rows) => all_policy_rows.extend(rows),
            Err(e) => {
                eprintln!("  Warning: failed to load Cedar policies from platform store: {e}");
            }
        }
    }
    if let Some(provider) = stack.turso.as_ref() {
        // Routed mode: also load legacy per-tenant policy blobs.
        for turso in provider.all_stores().await {
            match turso.load_tenant_policies().await {
                Ok(rows) => all_policy_rows.extend(rows),
                Err(e) => {
                    eprintln!("  Warning: failed to load Cedar policies from Turso store: {e}");
                }
            }
        }
    }

    // Legacy pass: populate from old `tenant_policies` table (per-tenant reload).
    if !all_policy_rows.is_empty() {
        let mut loaded_count = 0usize;
        for (tenant, policy_text) in &all_policy_rows {
            // Validate each tenant's policies individually so one bad tenant
            // doesn't prevent all others from loading.
            if let Err(e) = state
                .server
                .authz
                .reload_tenant_policies(tenant, policy_text)
            {
                eprintln!("  Warning: skipping invalid Cedar policies for tenant '{tenant}': {e}");
                continue;
            }
            // Update in-memory text cache.
            if let Ok(mut policies) = state.server.tenant_policies.write() {
                policies.insert(tenant.clone(), policy_text.clone());
            }
            loaded_count += 1;
        }
        if loaded_count > 0 {
            println!("  Restored Cedar policies for {loaded_count} tenants.");
        }
    }

    // New pass: load from `policies` table (per-entry rows with hash tracking).
    // Overwrites legacy data for any tenant that has entries in the new table.
    // `load_and_activate_tenant_policies` logs via tracing on success; no-ops silently.
    // Collect registered tenants; silently skip if registry lock is poisoned (unreachable in practice).
    let tenants: Vec<String> = state
        .server
        .registry
        .read()
        .map(|reg| {
            reg.tenant_ids()
                .into_iter()
                .map(|t| t.to_string())
                .collect()
        })
        .unwrap_or_default();

    for tenant in &tenants {
        load_and_activate_tenant_policies(&state.server, tenant).await;
    }
}

/// Phase 7: Recover WASM modules from persistent backend.
pub(super) async fn recover_wasm_modules(state: &PlatformState) {
    if state.server.storage_stack.is_none() {
        return;
    }
    match state.server.load_wasm_modules().await {
        Ok(count) if count > 0 => {
            println!("  Recovered {count} WASM modules from database.");
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("  Warning: failed to recover WASM modules: {e}");
        }
    }
}

/// Phase 7b: Recover encrypted secrets from persistent backend into in-memory cache.
pub(super) async fn recover_secrets(state: &PlatformState) {
    if state.server.secrets_vault.is_none() || state.server.storage_stack.is_none() {
        return;
    }

    // Collect known tenants from the registry.
    let tenants: Vec<String> = {
        let reg = state.registry.read().unwrap(); // ci-ok: infallible lock
        reg.tenant_ids()
            .into_iter()
            .map(|t| t.as_str().to_string())
            .collect()
    };

    let mut total = 0usize;
    for tenant in &tenants {
        match state.server.load_tenant_secrets(tenant).await {
            Ok(count) => total += count,
            Err(e) => {
                eprintln!("  Warning: failed to load secrets for tenant '{tenant}': {e}");
            }
        }
    }
    if total > 0 {
        println!("  Recovered {total} secrets from database.");
    }
}

/// Load the verification cache from Turso for a tenant (hash + verified status).
///
/// Routes to the per-tenant store in TenantRouted mode.
/// Returns an empty map if no Turso store is available.
async fn load_verified_cache(
    state: &PlatformState,
    tenant: &str,
) -> std::collections::BTreeMap<String, (String, bool)> {
    if let Some(turso) = state.server.turso_store_for_tenant(tenant).await {
        match turso.load_verification_cache(tenant).await {
            Ok(cache) => cache,
            Err(e) => {
                eprintln!("  Warning: failed to load verification cache for {tenant}: {e}");
                std::collections::BTreeMap::new()
            }
        }
    } else {
        std::collections::BTreeMap::new()
    }
}

/// Phase 8: Bootstrap system tenant and agent specs.
///
/// After verifying (or skipping via cache), persists spec hashes and
/// verification status to the per-tenant Turso store so subsequent boots
/// skip the cascade.
pub(super) async fn bootstrap_tenants(
    state: &PlatformState,
    apps: &[(String, String)],
    operator_tenant: &str,
) {
    let sys_cache = load_verified_cache(state, "temper-system").await;
    let sys_hashes = temper_platform::bootstrap_system_tenant(state, &sys_cache);
    if let Some(turso) = state.server.turso_store_for_tenant("temper-system").await {
        temper_platform::persist_system_verification(&turso, &sys_hashes, &sys_cache).await;
    }

    let default_cache = load_verified_cache(state, "default").await;
    let default_hashes =
        temper_platform::bootstrap_agent_specs(state, "default", false, &default_cache);
    if let Some(turso) = state.server.turso_store_for_tenant("default").await {
        temper_platform::persist_agent_verification(
            &turso,
            "default",
            &default_hashes,
            &default_cache,
        )
        .await;
    }

    if operator_tenant != "default" && !apps.iter().any(|(tenant, _)| tenant == operator_tenant) {
        let cache = load_verified_cache(state, operator_tenant).await;
        let hashes = temper_platform::bootstrap_agent_specs(state, operator_tenant, false, &cache);
        if let Some(turso) = state.server.turso_store_for_tenant(operator_tenant).await {
            temper_platform::persist_agent_verification(&turso, operator_tenant, &hashes, &cache)
                .await;
        }
    }

    for (tenant, _dir) in apps {
        let cache = load_verified_cache(state, tenant).await;
        // App tenants already have user specs loaded in Phase 2; merge the
        // built-in agent OS entities so we do not replace their entity-set map.
        let hashes = temper_platform::bootstrap_agent_specs(state, tenant, true, &cache);
        if let Some(turso) = state.server.turso_store_for_tenant(tenant).await {
            temper_platform::persist_agent_verification(&turso, tenant, &hashes, &cache).await;
        }
    }
    // In TenantRouted mode, bootstrap agent specs for all registered tenants.
    // OS app specs are already restored from the `specs` table by
    // `restore_registry_from_turso` (Phase 2) and Cedar policies by
    // `recover_cedar_policies` (Phase 6), so no reinstall loop is needed.
    if let Some(provider) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.turso.clone())
    {
        for tenant in provider.connected_tenants().await {
            let cache = load_verified_cache(state, &tenant).await;
            let hashes = temper_platform::bootstrap_agent_specs(state, &tenant, true, &cache);
            if let Some(turso) = state.server.turso_store_for_tenant(&tenant).await {
                temper_platform::persist_agent_verification(&turso, &tenant, &hashes, &cache).await;
            }
        }
    }

    // Register the bootstrap key only in the configured operator tenant. It
    // grants no implicit authority in other tenants (ADR-0157).
    if let Some(ref api_key) = state.api_token {
        temper_platform::bootstrap_operator_credential(state, api_key, operator_tenant).await;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppBootstrapSource {
    Persisted,
    Cli,
}

/// Phase 8b: Restore persisted local apps and apply explicit `--app` requests.
///
/// Why this exists:
/// - agent bootstrap (Phase 8) can replace tenant specs;
/// - App installs are durably tracked in `tenant_installed_apps`.
///
/// This phase replays persisted installs so app entities remain available
/// after restart, and then applies explicit CLI installs for `default`.
pub(super) async fn bootstrap_installed_apps(
    state: &PlatformState,
    apps: &[String],
) -> anyhow::Result<()> {
    let mut requested: BTreeMap<(String, String), AppBootstrapSource> = BTreeMap::new();

    if let Some(platform_store) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    {
        match platform_store.list_all_installed_apps().await {
            Ok(installed) => {
                for (tenant, app_name) in installed {
                    if platform_store
                        .get_installed_app(&tenant, &app_name)
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(|record| {
                            record.source_kind == "local_bundle"
                                || (record.source_kind == "genesis"
                                    && record.closure_id.starts_with("bundle:sha256:"))
                        })
                    {
                        continue;
                    }
                    requested.insert((tenant, app_name), AppBootstrapSource::Persisted);
                }
            }
            Err(e) => {
                eprintln!("  Warning: failed to load installed apps: {e}");
            }
        }
    }

    for app_name in apps {
        requested
            .entry(("default".to_string(), app_name.clone()))
            .and_modify(|source| *source = AppBootstrapSource::Cli)
            .or_insert(AppBootstrapSource::Cli);
    }

    if let Some(manifest_path) = std::env::var_os("TEMPER_BOOTSTRAP_MANIFEST")
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        let tenant = std::env::var("TEMPER_BOOTSTRAP_TENANT")
            .ok()
            .filter(|tenant| !tenant.trim().is_empty())
            .unwrap_or_else(|| "default".to_string());
        let result =
            temper_platform::os_apps::bootstrap_closure_manifest(state, &tenant, &manifest_path)
                .await
                .map_err(anyhow::Error::msg)?;
        println!(
            "  Bootstrap closure '{}' installed for '{}': {}",
            result.closure.id,
            tenant,
            result.installed_apps.join(", ")
        );
    }

    let restored_local_caches =
        temper_platform::app_bundles::restore_local_bundle_cache_roots(state)
            .await
            .map_err(anyhow::Error::msg)?;
    let restored_canonical_genesis_caches =
        temper_platform::app_bundles::restore_canonical_genesis_bundle_cache_roots(state)
            .await
            .map_err(anyhow::Error::msg)?;
    let restored_genesis_caches =
        temper_platform::genesis_install::restore_genesis_app_cache_roots(state).await;
    let restored_genesis_registry_caches =
        temper_platform::genesis_install::restore_genesis_registry_cache_roots(state).await;
    if restored_local_caches > 0
        || restored_genesis_caches > 0
        || restored_genesis_registry_caches > 0
        || restored_canonical_genesis_caches > 0
    {
        println!(
            "  Restored app cache roots: local={restored_local_caches}, canonical Genesis={restored_canonical_genesis_caches}, Genesis service={restored_genesis_caches}, Genesis registry={restored_genesis_registry_caches}"
        );
    }

    for ((tenant, app_name), source) in requested {
        if matches!(source, AppBootstrapSource::Persisted)
            && temper_platform::os_apps::get_os_app(&app_name).is_none()
        {
            use std::io::Write as _;

            let mut stderr = std::io::stderr();
            let _ = writeln!(
                stderr,
                "  Info: skipping persisted app '{app_name}' for '{tenant}': not present in configured app sources"
            );
            continue;
        }

        // Replay through the real installer every startup. Presence-only checks
        // are not sufficient because an installed app's bundle can drift:
        // updated IOA/Cedar/WASM/ADR assets must refresh even when the tenant
        // already has entity tables from an older install.
        match temper_platform::install_os_app(state, &tenant, &app_name).await {
            Ok(result) => match source {
                AppBootstrapSource::Persisted => {
                    let all: Vec<String> = result
                        .added
                        .iter()
                        .chain(&result.updated)
                        .chain(&result.skipped)
                        .cloned()
                        .collect();
                    println!(
                        "  Restored app '{app_name}' for '{tenant}': {}",
                        all.join(", ")
                    );
                }
                AppBootstrapSource::Cli => {
                    let all: Vec<String> = result
                        .added
                        .iter()
                        .chain(&result.updated)
                        .chain(&result.skipped)
                        .cloned()
                        .collect();
                    println!(
                        "  App '{app_name}' installed for '{tenant}': {}",
                        all.join(", ")
                    );
                }
            },
            Err(e) => {
                eprintln!("  Warning: failed to install app '{app_name}' for '{tenant}': {e}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use temper_platform::os_apps::get_os_app;
    use temper_platform::state::PlatformState;
    use temper_runtime::tenant::TenantId;
    use temper_server::storage::StorageStack;
    use temper_spec::csdl::parse_csdl;
    use temper_store_turso::TursoEventStore;

    use super::bootstrap_installed_apps;

    #[tokio::test]
    async fn bootstrap_installed_apps_replays_persisted_app_when_registry_specs_are_stale() {
        let tenant = "bootstrap-drift";
        let app_name = "temper-fs";
        let bundle = get_os_app(app_name).expect("temper-fs app bundle should load");
        let csdl_xml = bundle.csdl.clone().expect("temper-fs should have CSDL");
        let csdl = parse_csdl(&csdl_xml).expect("temper-fs CSDL should parse");
        let mut stale_specs: Vec<(String, String)> = bundle.specs.clone();
        stale_specs[0].1.push_str("\n# stale bootstrap copy\n");
        let stale_refs: Vec<(&str, &str)> = stale_specs
            .iter()
            .map(|(entity_type, source)| (entity_type.as_str(), source.as_str()))
            .collect();

        let mut state = PlatformState::new(None);
        {
            let mut registry = state.registry.write().unwrap();
            registry.register_tenant(tenant, csdl, csdl_xml, &stale_refs);
        }

        let db_path = std::env::temp_dir().join(format!(
            "temper-bootstrap-{}.db",
            temper_runtime::scheduler::sim_uuid()
        ));
        let db_url = format!("file:{}", db_path.display());
        let turso = TursoEventStore::new(&db_url, None)
            .await
            .expect("turso store should initialize");
        turso
            .record_installed_app(tenant, app_name)
            .await
            .expect("installed app should persist");
        state
            .server
            .set_storage_stack(StorageStack::from_turso(turso));

        bootstrap_installed_apps(&state, &[])
            .await
            .expect("bootstrap should replay persisted apps");

        let tenant_id = TenantId::new(tenant);
        let registry = state.registry.read().unwrap();
        let refreshed = registry
            .get_spec(&tenant_id, &bundle.specs[0].0)
            .expect("replayed app spec should remain registered");
        assert_eq!(
            refreshed.ioa_source, bundle.specs[0].1,
            "startup replay should refresh stale installed app specs from the current bundle"
        );

        let _ = std::fs::remove_file(&db_path);
    }
}
