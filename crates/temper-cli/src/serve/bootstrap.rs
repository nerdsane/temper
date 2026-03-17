//! Startup phase helpers for `temper serve`.
//!
//! Each function represents an explicit phase of the startup pipeline.
//! The `run` coordinator in `mod.rs` calls these in sequence.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::authz::load_and_activate_tenant_policies;
use temper_server::event_store::ServerEventStore;
use temper_server::registry::SpecRegistry;
use temper_server::registry_bootstrap::{
    restore_registry_from_postgres, restore_registry_from_turso,
};
use temper_server::webhooks::WebhookDispatcher;
use temper_store_redis::RedisEventStore;
use temper_store_turso::{TenantStoreRouter, TursoEventStore};

use crate::StorageBackend;

use super::loader::load_into_registry;
use super::storage::{
    connect_postgres_store, redact_connection_url, upsert_loaded_specs_to_postgres,
    upsert_loaded_specs_to_turso,
};

/// Phase 1: Initialize the storage backend (Postgres, Turso, Redis, or memory).
pub(super) async fn init_storage(
    storage: StorageBackend,
    storage_explicit: bool,
) -> Result<(Option<sqlx::PgPool>, Option<ServerEventStore>)> {
    let mut pg_pool: Option<sqlx::PgPool> = None;
    let event_store: Option<ServerEventStore> = match storage {
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
            if let Ok(platform_url) = std::env::var("TURSO_PLATFORM_URL") {
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
                Some(ServerEventStore::TenantRouted(router))
            } else {
                // Single-DB mode (local dev).
                let turso_url = match std::env::var("TURSO_URL") {
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
                };
                let turso_token = std::env::var("TURSO_AUTH_TOKEN").ok();
                let store = TursoEventStore::new(&turso_url, turso_token.as_deref())
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to connect to Turso/libSQL: {e}"))?;
                println!("  Storage: turso ({})", turso_url);
                Some(ServerEventStore::Turso(store))
            }
        }
        StorageBackend::Redis => {
            let redis_url = std::env::var("REDIS_URL")
                .context("REDIS_URL is required when --storage redis is selected")?;
            let store = RedisEventStore::new(&redis_url)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to connect to Redis: {e}"))?;
            println!("  Storage: redis ({})", redact_connection_url(&redis_url));
            Some(ServerEventStore::Redis(store))
        }
    };
    Ok((pg_pool, event_store))
}

/// Phase 2: Build the spec registry from persistent storage and disk apps.
pub(super) async fn build_registry(
    pg_pool: Option<&sqlx::PgPool>,
    event_store: &Option<ServerEventStore>,
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
    } else if let Some(ServerEventStore::Turso(turso)) = event_store {
        let restored = restore_registry_from_turso(&mut registry, turso)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        if restored > 0 {
            println!("  Restored {restored} specs from Turso.");
        }
    } else if let Some(ServerEventStore::TenantRouted(router)) = event_store {
        let mut total_restored = 0usize;
        let restored = restore_registry_from_turso(&mut registry, router.platform_store())
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        total_restored += restored;
        for tenant_id in router.connected_tenants().await {
            if let Ok(store) = router.store_for_tenant(&tenant_id).await {
                let restored = restore_registry_from_turso(&mut registry, &store)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                total_restored += restored;
            }
        }
        if total_restored > 0 {
            println!("  Restored {total_restored} specs from Turso (routed).");
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
        } else if let Some(ServerEventStore::Turso(turso)) = event_store {
            upsert_loaded_specs_to_turso(turso, tenant, &loaded).await?;
        } else if let Some(ServerEventStore::TenantRouted(router)) = event_store
            && let Ok(store) = router.store_for_tenant(tenant).await
        {
            upsert_loaded_specs_to_turso(&store, tenant, &loaded).await?;
        }
    }

    Ok((registry, tenant_policy_seed))
}

/// Phase 3: Auto-reload previously registered specs from `specs-registry.json`.
pub(super) fn auto_reload_specs(
    state: &PlatformState,
    data_dir: &Path,
) -> (usize, BTreeMap<String, String>) {
    let specs_registry_path = data_dir.join("specs-registry.json");
    let mut auto_reloaded = 0usize;
    let mut tenant_policy_seed = BTreeMap::new();

    if let Ok(content) = fs::read_to_string(&specs_registry_path)
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(entries) = value.as_object()
    {
        for (tenant, specs_dir) in entries {
            let Some(specs_dir) = specs_dir.as_str() else {
                continue;
            };

            let loaded = {
                let mut guard = state.registry.write().unwrap(); // ci-ok: infallible lock
                load_into_registry(&mut guard, specs_dir, tenant)
            };

            match loaded {
                Ok(loaded) => {
                    auto_reloaded += 1;
                    if let Some(text) = loaded.cedar_policy_text {
                        tenant_policy_seed.insert(tenant.clone(), text);
                    }
                }
                Err(e) => {
                    eprintln!(
                        "  Warning: failed to auto-reload app {tenant} from {specs_dir}: {e}"
                    );
                }
            }
        }
    }

    (auto_reloaded, tenant_policy_seed)
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
    if state.server.event_store.is_none() {
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
    for (tenant, _dir) in apps {
        let tenant_id = TenantId::new(tenant.as_str());
        if eager_hydrate {
            state.server.hydrate_from_store(&tenant_id).await;
        } else {
            state.server.populate_index_from_store(&tenant_id).await;
        }
    }
    // In TenantRouted mode, also hydrate all registered tenants.
    if let Some(ref store) = state.server.event_store
        && let Some(router) = store.tenant_router()
    {
        for tenant in router.connected_tenants().await {
            let tenant_id = TenantId::new(&tenant);
            if eager_hydrate {
                state.server.hydrate_from_store(&tenant_id).await;
            } else {
                state.server.populate_index_from_store(&tenant_id).await;
            }
        }
    }
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
    let Some(ref store) = state.server.event_store else {
        return;
    };

    let mut all_policy_rows: Vec<(String, String)> = Vec::new();

    if let Some(turso) = store.turso_store() {
        // Single-DB mode.
        match turso.load_tenant_policies().await {
            Ok(rows) => all_policy_rows.extend(rows),
            Err(e) => {
                eprintln!("  Warning: failed to load Cedar policies from Turso: {e}");
            }
        }
    } else if let Some(router) = store.tenant_router() {
        // Routed mode: load from platform store + each tenant store.
        match router.platform_store().load_tenant_policies().await {
            Ok(rows) => all_policy_rows.extend(rows),
            Err(e) => {
                eprintln!("  Warning: failed to load Cedar policies from platform store: {e}");
            }
        }
        for tenant_id in router.connected_tenants().await {
            if let Ok(turso) = router.store_for_tenant(&tenant_id).await {
                match turso.load_tenant_policies().await {
                    Ok(rows) => all_policy_rows.extend(rows),
                    Err(e) => {
                        eprintln!(
                            "  Warning: failed to load Cedar policies for tenant {tenant_id}: {e}"
                        );
                    }
                }
            }
        }
    }

    // Legacy pass: populate from old `tenant_policies` table.
    if !all_policy_rows.is_empty() {
        let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        let mut loaded_count = 0usize;
        for (tenant, policy_text) in &all_policy_rows {
            // Validate each tenant's policies individually so one bad tenant
            // doesn't prevent all others from loading.
            if temper_authz::AuthzEngine::new(policy_text).is_err() {
                eprintln!("  Warning: skipping invalid Cedar policies for tenant '{tenant}'");
                continue;
            }
            policies.insert(tenant.clone(), policy_text.clone());
            loaded_count += 1;
        }
        let mut combined = String::new();
        for text in policies.values() {
            combined.push_str(text);
            combined.push('\n');
        }
        if let Err(e) = state.server.authz.reload_policies(&combined) {
            eprintln!("  Warning: failed to reload Cedar policies: {e}");
        } else if loaded_count > 0 {
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

    if let Some(turso) = store.turso_store() {
        for tenant in &tenants {
            load_and_activate_tenant_policies(&state.server, tenant, turso).await;
        }
    } else if let Some(router) = store.tenant_router() {
        for tenant in &tenants {
            if let Ok(turso) = router.store_for_tenant(tenant).await {
                load_and_activate_tenant_policies(&state.server, tenant, &turso).await;
            }
        }
    }
}

/// Phase 7: Recover WASM modules from persistent backend.
pub(super) async fn recover_wasm_modules(state: &PlatformState) {
    if state.server.event_store.is_none() {
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

/// Load the verification cache from Turso for a tenant (hash + verified status).
///
/// Returns an empty map if no Turso store is available.
async fn load_verified_cache(
    state: &PlatformState,
    tenant: &str,
) -> std::collections::BTreeMap<String, (String, bool)> {
    if let Some(ref store) = state.server.event_store
        && let Some(turso) = store.platform_turso_store()
    {
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
/// verification status to Turso so subsequent boots skip the cascade.
pub(super) async fn bootstrap_tenants(state: &PlatformState, apps: &[(String, String)]) {
    // Resolve the Turso store once for persisting verification results.
    let turso = state
        .server
        .event_store
        .as_ref()
        .and_then(|s| s.platform_turso_store());

    let sys_cache = load_verified_cache(state, "temper-system").await;
    let sys_hashes = temper_platform::bootstrap_system_tenant(state, &sys_cache);
    if let Some(turso) = turso {
        temper_platform::persist_system_verification(turso, &sys_hashes).await;
    }

    let default_cache = load_verified_cache(state, "default").await;
    let default_hashes = temper_platform::bootstrap_agent_specs(state, "default", &default_cache);
    if let Some(turso) = turso {
        temper_platform::persist_agent_verification(turso, "default", &default_hashes).await;
    }

    for (tenant, _dir) in apps {
        let cache = load_verified_cache(state, tenant).await;
        let hashes = temper_platform::bootstrap_agent_specs(state, tenant, &cache);
        if let Some(turso) = turso {
            temper_platform::persist_agent_verification(turso, tenant, &hashes).await;
        }
    }
    // In TenantRouted mode, bootstrap agent specs for all registered tenants.
    // OS app specs are already restored from the `specs` table by
    // `restore_registry_from_turso` (Phase 2) and Cedar policies by
    // `recover_cedar_policies` (Phase 6), so no reinstall loop is needed.
    if let Some(ref store) = state.server.event_store
        && let Some(tenant_router) = store.tenant_router()
    {
        for tenant in tenant_router.connected_tenants().await {
            let cache = load_verified_cache(state, &tenant).await;
            let hashes = temper_platform::bootstrap_agent_specs(state, &tenant, &cache);
            if let Some(turso) = turso {
                temper_platform::persist_agent_verification(turso, &tenant, &hashes).await;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OsAppBootstrapSource {
    Persisted,
    Cli,
}

fn tenant_has_os_app_specs(state: &PlatformState, tenant: &str, app_name: &str) -> bool {
    let Some(bundle) = temper_platform::os_apps::get_os_app(app_name) else {
        return false;
    };
    let tenant_id = TenantId::new(tenant);
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    bundle
        .specs
        .iter()
        .all(|(entity_type, _)| registry.get_table(&tenant_id, entity_type).is_some())
}

/// Phase 8b: Restore persisted OS apps and apply `--os-app` requests.
///
/// Why this exists:
/// - agent bootstrap (Phase 8) can replace tenant specs;
/// - OS app installs are durably tracked in `tenant_installed_apps`.
///
/// This phase replays persisted installs so app entities remain available
/// after restart, and then applies explicit CLI installs for `default`.
pub(super) async fn bootstrap_installed_os_apps(state: &PlatformState, os_apps: &[String]) {
    let mut requested: BTreeMap<(String, String), OsAppBootstrapSource> = BTreeMap::new();

    if let Some(ref store) = state.server.event_store
        && let Some(turso) = store.platform_turso_store()
    {
        match turso.list_all_installed_apps().await {
            Ok(installed) => {
                for (tenant, app_name) in installed {
                    requested.insert((tenant, app_name), OsAppBootstrapSource::Persisted);
                }
            }
            Err(e) => {
                eprintln!("  Warning: failed to load installed OS apps: {e}");
            }
        }
    }

    for app_name in os_apps {
        requested
            .entry(("default".to_string(), app_name.clone()))
            .and_modify(|source| *source = OsAppBootstrapSource::Cli)
            .or_insert(OsAppBootstrapSource::Cli);
    }

    for ((tenant, app_name), source) in requested {
        if tenant_has_os_app_specs(state, &tenant, &app_name) {
            continue;
        }
        match temper_platform::install_os_app(state, &tenant, &app_name).await {
            Ok(result) => match source {
                OsAppBootstrapSource::Persisted => {
                    let all: Vec<String> = result
                        .added
                        .iter()
                        .chain(&result.updated)
                        .chain(&result.skipped)
                        .cloned()
                        .collect();
                    println!(
                        "  Restored OS app '{app_name}' for '{tenant}': {}",
                        all.join(", ")
                    );
                }
                OsAppBootstrapSource::Cli => {
                    let all: Vec<String> = result
                        .added
                        .iter()
                        .chain(&result.updated)
                        .chain(&result.skipped)
                        .cloned()
                        .collect();
                    println!(
                        "  OS app '{app_name}' installed for '{tenant}': {}",
                        all.join(", ")
                    );
                }
            },
            Err(e) => {
                eprintln!("  Warning: failed to install OS app '{app_name}' for '{tenant}': {e}");
            }
        }
    }
}
