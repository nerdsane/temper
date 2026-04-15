//! Platform-level DST harness.
//!
//! Orchestrates deterministic simulation of the full platform lifecycle using
//! **PRODUCTION code** (`install_os_app`, `dispatch_tenant_action`,
//! `recover_cedar_policies`, `restore_installed_apps`,
//! `restore_registry_from_platform_store`, `populate_index_from_store`)
//! with simulated storage backends.
//!
//! **FoundationDB principle:** Swap the I/O, keep the code. Every code path
//! in this harness — including `restart()` — calls the same production
//! functions that run in the CLI. No test-only reimplementations.
#![allow(dead_code)]
#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use temper_platform::os_apps::install_os_app;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::EntityResponse;
use temper_server::platform_store::{SimPlatformFaultConfig, SimPlatformStore};
use temper_server::registry_bootstrap::restore_registry_from_platform_store;
use temper_server::request_context::AgentContext;
use temper_server::{ServerEventStore, SpecRegistry};
use temper_store_sim::SimEventStore;
use temper_store_sim::SimFaultConfig;

/// Harness for platform-level deterministic simulation testing.
///
/// Holds both the durable simulated stores (which survive restarts) and the
/// current in-memory `PlatformState`. The `restart()` method drops in-memory
/// state and rebuilds from stores — using **production code paths**.
pub struct SimPlatformHarness {
    /// Simulated event store (durable across restarts).
    pub sim_event_store: SimEventStore,
    /// Simulated platform store for specs, policies, apps (durable across restarts).
    pub sim_platform_store: Arc<SimPlatformStore>,
    /// Current in-memory platform state (rebuilt on restart).
    pub platform_state: PlatformState,
    /// Seed used for deterministic RNG.
    pub seed: u64,
    /// Number of restarts performed.
    pub restart_count: u32,
}

impl SimPlatformHarness {
    /// Create a new harness with the given seed and fault configurations.
    pub fn new(
        seed: u64,
        event_faults: SimFaultConfig,
        platform_faults: SimPlatformFaultConfig,
    ) -> Self {
        let sim_event_store = SimEventStore::new(seed, event_faults);
        let sim_platform_store = Arc::new(SimPlatformStore::new(seed, platform_faults));

        let platform_state = PlatformState::new(None);
        let store = ServerEventStore::Sim(
            sim_event_store.clone(),
            Some(Arc::clone(&sim_platform_store)),
        );
        let mut state = platform_state;
        state.server.event_store = Some(Arc::new(store));

        Self {
            sim_event_store,
            sim_platform_store,
            platform_state: state,
            seed,
            restart_count: 0,
        }
    }

    /// Create a harness with no fault injection.
    pub fn no_faults(seed: u64) -> Self {
        Self::new(seed, SimFaultConfig::none(), SimPlatformFaultConfig::none())
    }

    /// Install an OS app using PRODUCTION code.
    pub async fn install_app(&self, tenant: &str, app_name: &str) -> Result<Vec<String>, String> {
        install_os_app(&self.platform_state, tenant, app_name)
            .await
            .map(|r| {
                let mut all = r.added;
                all.extend(r.updated);
                all.extend(r.skipped);
                all
            })
    }

    /// Override an existing entity's IOA spec inline (hot-swap).
    ///
    /// Useful for testing state machines in isolation without WASM integrations.
    /// The tenant and entity type must already be registered (via `install_skill`).
    pub fn register_inline_spec(&self, tenant: &str, entity_type: &str, ioa_source: &str) {
        let automaton =
            temper_spec::automaton::parse_automaton(ioa_source).expect("inline IOA should parse");
        let table = temper_jit::table::TransitionTable::from_automaton(&automaton);
        let mut registry = self.platform_state.server.registry.write().unwrap(); // ci-ok: infallible lock
        let spec = registry
            .get_spec_mut(&TenantId::new(tenant), entity_type)
            .unwrap_or_else(|| {
                panic!("entity type '{entity_type}' not found for tenant '{tenant}'")
            });
        spec.swap_controller().swap(table);
        spec.integrations = automaton.integrations;
        spec.ioa_source = ioa_source.to_string();
    }

    /// Dispatch an action using PRODUCTION code.
    pub async fn dispatch(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        self.platform_state
            .server
            .dispatch_tenant_action(
                &TenantId::new(tenant),
                entity_type,
                entity_id,
                action,
                params,
                &AgentContext::default(),
            )
            .await
    }

    /// Simulate a restart: drop in-memory state, rebuild from stores.
    ///
    /// Runs **production code** — the same functions that execute during
    /// CLI bootstrap:
    /// 1. Create fresh `PlatformState`
    /// 2. Wire the same durable stores
    /// 3. [`restore_registry_from_platform_store`] — production spec recovery
    /// 4. [`temper_platform::recovery::recover_cedar_policies`] — production Cedar recovery
    /// 5. [`temper_platform::recovery::restore_installed_apps`] — production app recovery
    /// 6. [`populate_index_from_store`] — production index population
    pub async fn restart(&mut self) {
        self.restart_count += 1;

        // 1. Fresh PlatformState with empty registry.
        let mut new_state = PlatformState::new(None);

        // 2. Wire same durable stores.
        let store = ServerEventStore::Sim(
            self.sim_event_store.clone(),
            Some(Arc::clone(&self.sim_platform_store)),
        );
        new_state.server.event_store = Some(Arc::new(store));

        // 3. Restore specs from platform store — PRODUCTION code.
        {
            let mut registry = new_state.registry.write().unwrap(); // ci-ok: infallible lock
            let _restored = restore_registry_from_platform_store(
                &mut registry,
                self.sim_platform_store.as_ref(),
            )
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("Failed to restore registry from platform store: {e}");
                0
            });
        }

        // 4. Recover Cedar policies — PRODUCTION code.
        temper_platform::recovery::recover_cedar_policies(
            &new_state,
            self.sim_platform_store.as_ref(),
        )
        .await;

        // 5. Restore installed skills — PRODUCTION code.
        temper_platform::recovery::restore_installed_apps(
            &new_state,
            self.sim_platform_store.as_ref(),
        )
        .await;

        // 6. Populate entity index from event store — PRODUCTION code.
        // Discover tenants from the registry (which was just restored).
        let tenant_ids: Vec<TenantId> = {
            let registry = new_state.registry.read().unwrap(); // ci-ok: infallible lock
            registry.tenant_ids().into_iter().cloned().collect()
        };
        for tenant_id in &tenant_ids {
            new_state.server.populate_index_from_store(tenant_id).await;
        }

        self.platform_state = new_state;
    }

    /// Read-only access to the spec registry.
    pub fn registry(&self) -> &std::sync::Arc<std::sync::RwLock<SpecRegistry>> {
        &self.platform_state.registry
    }

    /// Temporarily disable fault injection on the platform store,
    /// run the given async closure, then restore faults.
    ///
    /// Invariant checks must read the store reliably — faults would cause
    /// spurious check failures that aren't real invariant violations.
    pub async fn with_faults_disabled<F, Fut, T>(&self, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let prev = self.sim_platform_store.disable_faults();
        let result = f().await;
        self.sim_platform_store.restore_faults(prev);
        result
    }
}
