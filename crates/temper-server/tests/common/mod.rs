//! Shared DST test helpers.
//!
//! Provides common builders, fixtures, and dispatch helpers to eliminate
//! duplicated scaffolding across DST test suites.

use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::dispatch::AgentContext;
use temper_server::registry::SpecRegistry;
use temper_server::{ServerEventStore, ServerState};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

// ── Shared fixtures ─────────────────────────────────────────────────────

pub const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
pub const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

// ── State builders ──────────────────────────────────────────────────────

/// Build a `ServerState` with a single tenant (default), Order spec, and
/// simulated persistence.  Returns the state and the sim store handle for
/// fault injection.
pub fn build_default_state(seed: u64, system_name: &str) -> (ServerState, SimEventStore) {
    build_single_tenant_state(seed, system_name, "default", &[("Order", ORDER_IOA)])
}

/// Build a `ServerState` for a single tenant with the given entity specs.
pub fn build_single_tenant_state(
    seed: u64,
    system_name: &str,
    tenant: &str,
    entities: &[(&str, &str)],
) -> (ServerState, SimEventStore) {
    let sim_store = SimEventStore::no_faults(seed);
    let store = ServerEventStore::Sim(sim_store.clone());

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(tenant, csdl, CSDL_XML.to_string(), entities);

    let system = ActorSystem::new(system_name);
    let mut state = ServerState::from_registry(system, registry);
    state.event_store = Some(Arc::new(store));
    (state, sim_store)
}

/// Build a `ServerState` with two tenants for isolation tests.
pub fn build_two_tenant_state(
    seed: u64,
    system_name: &str,
    tenant_a: &str,
    entities_a: &[(&str, &str)],
    tenant_b: &str,
    entities_b: &[(&str, &str)],
) -> ServerState {
    let sim_store = SimEventStore::no_faults(seed);
    let store = ServerEventStore::Sim(sim_store);

    let mut registry = SpecRegistry::new();
    let csdl_a = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(tenant_a, csdl_a, CSDL_XML.to_string(), entities_a);

    let csdl_b = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(tenant_b, csdl_b, CSDL_XML.to_string(), entities_b);

    let system = ActorSystem::new(system_name);
    let mut state = ServerState::from_registry(system, registry);
    state.event_store = Some(Arc::new(store));
    state
}

// ── Dispatch helpers ────────────────────────────────────────────────────

/// Dispatch an action with default agent context and no integration await.
pub async fn dispatch(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
) -> Result<temper_server::entity_actor::EntityResponse, String> {
    state
        .dispatch_tenant_action(
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            &AgentContext::default(),
        )
        .await
}
