//! Startup phase helpers for `temper serve`.
//!
//! Each function represents an explicit phase of the startup pipeline.
//! The `run` coordinator in `mod.rs` calls these in sequence.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::authz::load_and_activate_tenant_policies;
use temper_server::registry::SpecRegistry;
use temper_server::registry_bootstrap::{
    restore_registry_from_postgres, restore_registry_from_turso,
};
use temper_server::storage::{StorageRoom, StorageStack};
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
///
/// Also returns the local Turso database when `TURSO_SYNC_URL` gives it a
/// Turso Cloud copy; the server answers changes only once they reach it.
pub(super) async fn init_storage(
    storage: StorageBackend,
    storage_explicit: bool,
) -> Result<(
    Option<sqlx::PgPool>,
    Option<StorageStack>,
    Option<TursoEventStore>,
)> {
    let mut pg_pool: Option<sqlx::PgPool> = None;
    let mut synced_store: Option<TursoEventStore> = None;
    if !matches!(storage, StorageBackend::Turso) && std::env::var_os("TURSO_SYNC_URL").is_some() {
        anyhow::bail!("TURSO_SYNC_URL needs --storage turso");
    }
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
            if let Ok(platform_url) = std::env::var("TURSO_PLATFORM_URL") {
                if std::env::var_os("TURSO_SYNC_URL").is_some() {
                    anyhow::bail!(
                        "TURSO_SYNC_URL works with one local database, not TURSO_PLATFORM_URL"
                    );
                }
                let platform_token = std::env::var("TURSO_PLATFORM_AUTH_TOKEN").ok();
                let local_base_dir = std::env::var("TURSO_LOCAL_BASE_DIR").ok().or_else(|| {
                    let home = std::env::var("HOME").ok()?;
                    Some(format!("{home}/.local/share/temper/tenants"))
                });

                let room = local_base_dir.clone().map(StorageRoom::on_disk);
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
                Some(with_room(StorageStack::from_tenant_router(router), room))
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
                let sync_url = std::env::var("TURSO_SYNC_URL")
                    .ok()
                    .filter(|url| !url.is_empty());
                let (store, copied) = if let Some(sync_url) = sync_url {
                    if turso_url.starts_with("libsql://") {
                        anyhow::bail!("TURSO_SYNC_URL needs a local TURSO_URL (file:...)");
                    }
                    let sync_token = std::env::var("TURSO_SYNC_AUTH_TOKEN")
                        .context("TURSO_SYNC_AUTH_TOKEN is required with TURSO_SYNC_URL")?;
                    let store = TursoEventStore::new_synced(&turso_url, &sync_url, &sync_token)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to open synced Turso database: {e}")
                        })?;
                    synced_store = Some(store.clone());
                    let copied =
                        format!(", each change copied to {sync_url} before it is answered");
                    (store, copied)
                } else {
                    let turso_token = std::env::var("TURSO_AUTH_TOKEN").ok();
                    let store = TursoEventStore::new(&turso_url, turso_token.as_deref())
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to connect to Turso/libSQL: {e}"))?;
                    (store, String::new())
                };
                println!("  Storage: turso ({turso_url}){copied}");
                let room = local_store_dir(&turso_url).map(StorageRoom::on_disk);
                Some(with_room(StorageStack::from_turso(store), room))
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
    Ok((pg_pool, storage_stack, synced_store))
}

/// The directory a local database at `url` (`file:` or a bare path) lives in;
/// a remote one (`libsql://`) has none.
fn local_store_dir(url: &str) -> Option<std::path::PathBuf> {
    if url.starts_with("libsql://") {
        return None;
    }
    let path = Path::new(url.strip_prefix("file:").unwrap_or(url));
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    Some(dir.unwrap_or(Path::new(".")).to_path_buf())
}

/// A stack whose store lives on a local disk refuses appends while that disk
/// is under the floor (`temper_server::storage::STORAGE_FLOOR_BYTES`).
fn with_room(stack: StorageStack, room: Option<StorageRoom>) -> StorageStack {
    match room {
        Some(room) => stack.with_storage_room(room),
        None => stack,
    }
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
    for (tenant, reason) in registry.unavailable_tenants() {
        tracing::warn!(tenant = %tenant, %reason, "tenant is unavailable, its stored specs did not load");
    }

    // Load app specs from disk, persisting to backend.
    for (tenant, specs_dir) in apps {
        let unavailable = registry
            .tenant_unavailable_reason(&TenantId::new(tenant))
            .is_some();
        if skip_if_marked(unavailable, tenant, &format!("specs from {specs_dir}")) {
            continue;
        }
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

/// Whether startup must leave `tenant` alone, printing one line if so.
///
/// A tenant whose stored specs did not load keeps its stored rows exactly as
/// they are, for repair; every startup step that would write for it skips it.
fn skip_unavailable_tenant(state: &PlatformState, tenant: &str, what: &str) -> bool {
    skip_if_marked(
        state.tenant_unavailable_reason(tenant).is_some(),
        tenant,
        what,
    )
}

fn skip_if_marked(unavailable: bool, tenant: &str, what: &str) -> bool {
    if !unavailable {
        return false;
    }
    tracing::warn!(
        tenant = %tenant,
        "skipping {what}: the tenant is unavailable, its stored rows are kept for repair"
    );
    true
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

/// What a boot does with the operator credentials earlier boots issued for other keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EarlierOperatorKeys {
    /// Keep them: another holder may still use an older key.
    Keep,
    /// Revoke them: this deployment is the only holder of its key (a Katagami
    /// app's Temper, keyed by its control plane), so a key it no longer holds
    /// stops working. Set by `TEMPER_RETIRE_OLD_OPERATOR_KEYS=1`.
    Retire,
}

impl EarlierOperatorKeys {
    /// The policy `TEMPER_RETIRE_OLD_OPERATOR_KEYS` sets for this boot.
    pub(super) fn from_env() -> Self {
        let retire = std::env::var("TEMPER_RETIRE_OLD_OPERATOR_KEYS").is_ok_and(|v| v == "1"); // determinism-ok: read once at startup
        if retire { Self::Retire } else { Self::Keep }
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
    earlier_operator_keys: EarlierOperatorKeys,
) {
    let sys_cache = load_verified_cache(state, "temper-system").await;
    let sys_hashes = temper_platform::bootstrap_system_tenant(state, &sys_cache);
    if let Some(turso) = state.server.turso_store_for_tenant("temper-system").await {
        temper_platform::persist_system_verification(&turso, &sys_hashes, &sys_cache).await;
    }

    // Every tenant that receives the agent specs also needs the trusted issuer
    // registered, or issuer-signed JWTs 401 there: bearer resolution looks the
    // issuer up in the REQUEST's tenant, not in `default`. App tenants already
    // have user specs loaded in Phase 2. In TenantRouted mode every connected
    // tenant gets them too: its OS app specs were restored from the `specs`
    // table in Phase 2 and its Cedar policies in Phase 6, so no reinstall loop
    // is needed. Every tenant merges (`default` included), so specs restored
    // in Phase 2 keep their entity sets.
    let mut agent_spec_tenants: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    agent_spec_tenants.insert("default".to_string());
    agent_spec_tenants.extend(apps.iter().map(|(tenant, _dir)| tenant.clone()));
    if let Some(provider) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.turso.clone())
    {
        agent_spec_tenants.extend(provider.connected_tenants().await);
    }
    agent_spec_tenants.retain(|tenant| !skip_unavailable_tenant(state, tenant, "agent specs"));

    for tenant in &agent_spec_tenants {
        let cache = load_verified_cache(state, tenant).await;
        let hashes = temper_platform::bootstrap_agent_specs(state, tenant, true, &cache);
        if let Some(turso) = state.server.turso_store_for_tenant(tenant).await {
            temper_platform::persist_agent_verification(&turso, tenant, &hashes, &cache).await;
        }
    }

    // Register the bootstrap key as a normal operator credential in the default
    // tenant. It grants no implicit authority in other tenants (ADR-0157).
    if let Some(ref api_key) = state.api_token
        && agent_spec_tenants.contains("default")
        && let Err(e) =
            temper_platform::bootstrap_operator_credential(state, api_key, "default").await
    {
        eprintln!("  Warning: failed to bootstrap operator credential for tenant 'default': {e}");
    }
    // A deployment that is the only holder of its key (a Katagami app's Temper,
    // keyed by its control plane) retires the credentials earlier boots issued
    // for other keys, so a key it no longer holds stops working.
    if earlier_operator_keys == EarlierOperatorKeys::Retire
        && let Some(ref api_key) = state.api_token
    {
        temper_platform::retire_other_operator_credentials(state, api_key, "default").await;
    }

    // Register a trusted JWT issuer from env config, if provided (ARN-255).
    // This is how a deployment activates the platform-issued-token path without
    // an authenticated API call. Registered for every tenant that carries the
    // agent specs, since the issuer is resolved in the request's own tenant.
    for tenant in &agent_spec_tenants {
        if let Err(e) = temper_platform::bootstrap_trusted_issuer_from_env(state, tenant).await {
            eprintln!("  Warning: failed to bootstrap trusted issuer for tenant '{tenant}': {e}");
        }
    }

    // ADR-0069: every tenant has the platform's HttpEndpoint type, so its routes
    // have somewhere to live from its first boot.
    for tenant in &agent_spec_tenants {
        if let Err(e) = temper_platform::bootstrap_http_endpoint_spec(state, tenant) {
            tracing::warn!(tenant = %tenant, error = %e, "failed to register HttpEndpoint");
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppBootstrapSource {
    Persisted,
    Cli,
}

/// What boot prints for an `--app` it has no files for: the specs it stored
/// for the app still serve, or, with none stored, it serves nothing for the
/// app and waits for its first install.
pub(super) fn missing_app_line(app: &str, tenant: &str, stored: bool) -> String {
    if stored {
        temper_platform::os_apps::stored_line(app, tenant)
    } else {
        temper_platform::os_apps::waiting_line(app, tenant)
    }
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
                    requested.insert((tenant, app_name), AppBootstrapSource::Persisted);
                }
            }
            Err(e) => {
                eprintln!("  Warning: failed to load installed apps: {e}");
            }
        }
    }

    // Apps whose install this server stored: their specs come back from its
    // database at boot, so it serves them even with no files for them anywhere.
    let stored: BTreeSet<(String, String)> = requested.keys().cloned().collect();

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
        if !skip_unavailable_tenant(state, &tenant, "the bootstrap closure") {
            let result = temper_platform::os_apps::bootstrap_closure_manifest(
                state,
                &tenant,
                &manifest_path,
            )
            .await
            .map_err(anyhow::Error::msg)?;
            println!(
                "  Bootstrap closure '{}' installed for '{}': {}",
                result.closure.id,
                tenant,
                result.installed_apps.join(", ")
            );
        }
    }

    let restored_genesis_caches =
        temper_platform::genesis_install::restore_genesis_app_cache_roots(state).await;
    let restored_genesis_registry_caches =
        temper_platform::genesis_install::restore_genesis_registry_cache_roots(state).await;
    if restored_genesis_caches > 0 || restored_genesis_registry_caches > 0 {
        println!(
            "  Restored Genesis app cache roots: service={restored_genesis_caches}, registry={restored_genesis_registry_caches}"
        );
    }
    for ((tenant, app_name), source) in requested {
        if skip_unavailable_tenant(state, &tenant, &format!("app '{app_name}'")) {
            continue;
        }
        if matches!(source, AppBootstrapSource::Persisted)
            && temper_platform::os_apps::tenant_os_app(&tenant, &app_name).is_none()
        {
            use std::io::Write as _;

            let mut stderr = std::io::stderr();
            let _ = writeln!(
                stderr,
                "  Info: skipping persisted app '{app_name}' for '{tenant}': not present in configured app sources"
            );
            continue;
        }
        // An app asked for by `--app` with no files anywhere yet has nothing to
        // install until its first push (`POST /api/os-apps/{name}/install`);
        // from that push on, its pushed version is what resolves here.
        if matches!(source, AppBootstrapSource::Cli)
            && temper_platform::os_apps::tenant_os_app(&tenant, &app_name).is_none()
        {
            let was_stored = stored.contains(&(tenant.clone(), app_name.clone()));
            println!("{}", missing_app_line(&app_name, &tenant, was_stored));
            continue;
        }

        // Replay through the real installer every startup. Presence-only checks
        // are not sufficient because an installed app's bundle can drift:
        // updated IOA/Cedar/WASM/ADR assets must refresh even when the tenant
        // already has entity tables from an older install. A replay of a stored
        // install keeps its recorded origin; only an `--app` request is a new
        // install. An `--app` whose stored install is a pushed version replays
        // it too: the restore above made that version what the app resolves,
        // and a new install would record it as the catalog's files.
        let install = match source {
            AppBootstrapSource::Persisted => {
                temper_platform::restore_os_app(state, &tenant, &app_name).await
            }
            AppBootstrapSource::Cli
                if temper_platform::genesis_install::pushed_install_recorded(
                    state, &tenant, &app_name,
                )
                .await =>
            {
                temper_platform::restore_os_app(state, &tenant, &app_name).await
            }
            AppBootstrapSource::Cli => {
                temper_platform::install_os_app(state, &tenant, &app_name).await
            }
        };
        match install {
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
                    // A start's supervisor reads the notes from these lines, so they wait for Jev.
                    let result = result.with_reviews_read().await;
                    if !result.warnings.is_empty() || !result.modules_reviewed.is_empty() {
                        print_install_notes(
                            &app_name,
                            &serde_json::json!({
                                "warnings": result.warnings,
                                "modules_reviewed": result.modules_reviewed,
                            }),
                        );
                    }
                    println!(
                        "{}",
                        temper_platform::os_apps::installed_line(&app_name, &tenant, &result)
                    );
                    // The version just served from the boot catalog becomes the
                    // app's first pushed version when it has none, so every
                    // later boot serves it until the next push, whatever the
                    // catalog holds then (see `adopt_catalog_app`).
                    match temper_platform::genesis_install::adopt_catalog_app(
                        state, &tenant, &app_name,
                    )
                    .await
                    {
                        Ok(Some(digest)) => tracing::info!(
                            tenant = %tenant,
                            app = %app_name,
                            %digest,
                            "adopted the boot catalog's version as the first pushed version"
                        ),
                        Ok(None) => {}
                        Err(error) => tracing::error!(
                            tenant = %tenant,
                            app = %app_name,
                            %error,
                            "the served version was not adopted; the next boot serves the catalog's"
                        ),
                    }
                }
            },
            Err(e) => {
                if matches!(source, AppBootstrapSource::Cli) {
                    print_install_notes(&app_name, &serde_json::json!({ "refused": e }));
                }
                // A stored install that does not come back is an outage of that
                // app, so it is logged as an error; a `--app` request only warns.
                let message = match source {
                    AppBootstrapSource::Persisted => {
                        tracing::error!(
                            tenant = %tenant,
                            app = %app_name,
                            error = %e,
                            "Stored app install was not restored"
                        );
                        format!("Error: stored app '{app_name}' for '{tenant}' was not restored")
                    }
                    AppBootstrapSource::Cli => {
                        format!("Warning: failed to install app '{app_name}' for '{tenant}'")
                    }
                };
                eprintln!("  {message}: {e}");
            }
        }
    }

    Ok(())
}

/// Everything the install of an app given on the command line said, as one compact JSON object on one line before the
/// line that says it installed or failed: its warnings and the modules Jev reviewed, or why it was refused, whole
/// (`refused`). A reader of a start's output that sees the verdict already has it (Katagami's start wrapper keeps both
/// lines and passes this one on as it is).
fn print_install_notes(app_name: &str, line: &serde_json::Value) {
    use std::io::Write as _;

    let _ = writeln!(
        std::io::stdout(),
        "  App '{app_name}' install notes: {line}"
    );
}

#[cfg(test)]
#[path = "bootstrap_test.rs"]
mod tests;
