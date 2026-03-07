//! Startup phase helpers for `temper serve`.
//!
//! Each function represents an explicit phase of the startup pipeline.
//! The `run` coordinator in `mod.rs` calls these in sequence.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
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

                let router = TenantStoreRouter::new(
                    &platform_url,
                    platform_token.as_deref(),
                    local_base_dir,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Failed to create tenant store router: {e}"))?;

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
) -> Result<SpecRegistry> {
    let mut registry = SpecRegistry::new();

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
    } else if let Some(ServerEventStore::TenantRouted(ref router)) = event_store {
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
        if let Some(pool) = pg_pool {
            upsert_loaded_specs_to_postgres(pool, tenant, &loaded).await?;
        } else if let Some(ServerEventStore::Turso(turso)) = event_store {
            upsert_loaded_specs_to_turso(turso, tenant, &loaded).await?;
        } else if let Some(ServerEventStore::TenantRouted(ref router)) = event_store {
            if let Ok(store) = router.store_for_tenant(tenant).await {
                upsert_loaded_specs_to_turso(&store, tenant, &loaded).await?;
            }
        }
    }

    Ok(registry)
}

/// Phase 3: Auto-reload previously registered specs from `specs-registry.json`.
pub(super) fn auto_reload_specs(state: &PlatformState, data_dir: &Path) -> usize {
    let specs_registry_path = data_dir.join("specs-registry.json");
    let mut auto_reloaded = 0usize;

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
                Ok(_) => auto_reloaded += 1,
                Err(e) => {
                    eprintln!(
                        "  Warning: failed to auto-reload app {tenant} from {specs_dir}: {e}"
                    );
                }
            }
        }
    }

    auto_reloaded
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
    for (tenant, _dir) in apps {
        let tenant_id = TenantId::new(tenant.as_str());
        state.server.hydrate_from_store(&tenant_id).await;
    }
    // In TenantRouted mode, also hydrate all registered tenants.
    if let Some(ref store) = state.server.event_store
        && let Some(router) = store.tenant_router()
    {
        for tenant in router.connected_tenants().await {
            let tenant_id = TenantId::new(&tenant);
            state.server.hydrate_from_store(&tenant_id).await;
        }
    }
}

/// Phase 6: Recover Cedar policies from persistent storage.
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

    if !all_policy_rows.is_empty() {
        let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        for (tenant, policy_text) in &all_policy_rows {
            policies.insert(tenant.clone(), policy_text.clone());
        }
        let mut combined = String::new();
        for text in policies.values() {
            combined.push_str(text);
            combined.push('\n');
        }
        if let Err(e) = state.server.authz.reload_policies(&combined) {
            eprintln!("  Warning: failed to reload Cedar policies: {e}");
        } else {
            println!(
                "  Restored Cedar policies for {} tenants.",
                all_policy_rows.len()
            );
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

/// Phase 8: Bootstrap system tenant and agent specs.
pub(super) async fn bootstrap_tenants(state: &PlatformState, apps: &[(String, String)]) {
    temper_platform::bootstrap_system_tenant(state);
    temper_platform::bootstrap_agent_specs(state, "default");
    for (tenant, _dir) in apps {
        temper_platform::bootstrap_agent_specs(state, tenant);
    }
    // In TenantRouted mode, bootstrap agent specs for all registered tenants.
    if let Some(ref store) = state.server.event_store
        && let Some(tenant_router) = store.tenant_router()
    {
        for tenant in tenant_router.connected_tenants().await {
            temper_platform::bootstrap_agent_specs(state, &tenant);
        }
    }
}
