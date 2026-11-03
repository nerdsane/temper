//! What `temper serve` does between opening its store and serving.
//!
//! A server with many tenants serves before it restores them all.
//! [`read_stored`] reads the specs the store holds and registers the server's
//! own tenants' ([`Later::own_tenants`]), [`restore_own`] brings the rest of
//! their state back, and [`start_serving`] starts the runtime just before the
//! listener answers, and the walk that restores every other tenant after it
//! (`super::restore`). What a boot reads before it serves therefore does not
//! grow with the tenants it holds, except the one read per store of every
//! tenant's spec rows and recorded installs.

use std::collections::BTreeMap;

use anyhow::Result;

use temper_platform::state::PlatformState;
use temper_server::registry::SpecRegistry;
use temper_server::storage::StorageStack;

use super::bootstrap;
use super::installed_apps;
use super::later::Later;
use super::restore;

/// What a boot read from its store before it serves.
pub(super) struct Stored {
    /// The registry with the server's own tenants restored.
    pub registry: SpecRegistry,
    /// Cedar policy text of the `--app` tenants loaded from disk.
    pub policy_seed: BTreeMap<String, String>,
    /// The tenants restored after the server serves.
    pub later: Later,
}

/// Read the store's specs into a registry, and the `--app` specs from disk:
/// the server's own tenants registered, every other tenant's rows left for
/// after it serves.
pub(super) async fn read_stored(
    pg_pool: Option<&sqlx::PgPool>,
    storage_stack: Option<&StorageStack>,
    apps: &[(String, String)],
) -> Result<Stored> {
    let bootstrap_tenant = std::env::var("TEMPER_BOOTSTRAP_TENANT") // determinism-ok: read once at startup
        .ok()
        .filter(|tenant| !tenant.trim().is_empty());
    let mut later = Later::new(Later::own_tenants(apps, bootstrap_tenant.as_deref()));
    let (registry, policy_seed) =
        bootstrap::build_registry(pg_pool, storage_stack, apps, &mut later).await?;
    Ok(Stored {
        registry,
        policy_seed,
        later,
    })
}

/// Bring the server's own tenants back: the `--app` tenants' Cedar from disk
/// (`policy_seed`), their records, Cedar, modules, secrets, the system and
/// agent specs, and their installed apps. What any other tenant needs is left
/// in `later`.
pub(super) async fn restore_own(
    state: &PlatformState,
    policy_seed: BTreeMap<String, String>,
    apps: &[(String, String)],
    os_app_installs: &[String],
    later: &mut Later,
) -> Result<()> {
    bootstrap::seed_cedar_policies(state, policy_seed);
    state.server.rebuild_reaction_dispatcher();
    bootstrap::hydrate_entities(state, apps, later).await;
    bootstrap::recover_cedar_policies(state, later).await;
    bootstrap::recover_wasm_modules(state, later).await;
    bootstrap::recover_secrets(state).await;
    let keys = bootstrap::EarlierOperatorKeys::from_env();
    bootstrap::bootstrap_tenants(state, apps, keys, later).await;
    installed_apps::bootstrap_installed_apps(state, os_app_installs, later).await
}

/// Start the runtime the server serves with: the HttpEndpoint route tables
/// and their reconciler, and the state timers re-armed from their rows; then
/// the walk that restores the tenants `later` holds, which are starting
/// until each is restored.
pub(super) async fn start_serving(state: &PlatformState, later: Later) {
    // Subscribes to entity state changes; rebuilds the per-tenant route table
    // on every HttpEndpoint transition (ADR-0056 Phase 2 slice 3b).
    temper_server::http_endpoint::spawn_reconciler(state.server.clone());
    // Prime the route tables, so HttpEndpoint rows already in the store are
    // routable on the first request (the reconciler only reacts to changes).
    let tenants: Vec<temper_runtime::tenant::TenantId> = {
        let reg = state
            .server
            .registry
            .read()
            .expect("registry lock poisoned");
        reg.tenant_ids().iter().map(|t| (*t).clone()).collect()
    };
    for tenant in tenants {
        temper_server::http_endpoint::rebuild_tenant_table(&state.server, &tenant).await;
    }
    // State timers re-armed from their rows (ADR-0056) before the first
    // request can act on their records, and runtime metrics.
    let _boot_timers = state.server.start_runtime().await;
    // Last, with nothing awaited after it: no other tenant is read before the
    // server serves.
    restore::spawn_walk(state.clone(), later);
}
