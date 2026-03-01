//! DST actor lifecycle tests.
//!
//! Verifies entity actor lifecycle with simulated persistence:
//! - Create → dispatch → persist → crash → respawn → replay → continue
//! - Full lifecycle through all states with persistence

use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::dispatch::AgentContext;
use temper_server::registry::SpecRegistry;
use temper_server::{ServerEventStore, ServerState};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn build_state_with_sim(seed: u64) -> (ServerState, SimEventStore) {
    let sim_store = SimEventStore::no_faults(seed);
    let store = ServerEventStore::Sim(sim_store.clone());

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );

    let system = ActorSystem::new("dst-lifecycle");
    let mut state = ServerState::from_registry(system, registry);
    state.event_store = Some(Arc::new(store));
    (state, sim_store)
}

// =========================================================================
// Test: Full order lifecycle through dispatch_tenant_action
// =========================================================================

#[tokio::test]
async fn dst_full_lifecycle_through_dispatch() {
    for seed in 0..100 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let (state, _sim_store) = build_state_with_sim(seed);
        let tenant = TenantId::default();
        let agent = AgentContext::default();
        let eid = format!("o-life-{seed}");

        let actions_and_expected = [
            ("AddItem", "Draft"),
            ("SubmitOrder", "Submitted"),
            ("ConfirmOrder", "Confirmed"),
            ("ProcessOrder", "Processing"),
            ("ShipOrder", "Shipped"),
            ("DeliverOrder", "Delivered"),
        ];

        for (action, expected_status) in &actions_and_expected {
            let r = state
                .dispatch_tenant_action(
                    &tenant,
                    "Order",
                    &eid,
                    action,
                    serde_json::json!({}),
                    &agent,
                )
                .await
                .unwrap_or_else(|e| panic!("seed {seed}: {action} failed: {e}"));
            assert!(
                r.success,
                "seed {seed}: {action} not successful: {:?}",
                r.error
            );
            assert_eq!(
                &r.state.status, expected_status,
                "seed {seed}: after {action}"
            );
        }
    }
}

// =========================================================================
// Test: Crash and recovery through ServerState dispatch
// =========================================================================

#[tokio::test]
async fn dst_lifecycle_crash_and_recovery() {
    for seed in 0..50 {
        let sim_store = SimEventStore::no_faults(seed);
        let store = Arc::new(ServerEventStore::Sim(sim_store.clone()));
        let tenant = TenantId::default();
        let agent = AgentContext::default();
        let eid = format!("o-crash-{seed}");

        // Phase 1: Create and advance to Confirmed.
        {
            let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
            let mut registry = SpecRegistry::new();
            let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
            registry.register_tenant(
                "default",
                csdl,
                CSDL_XML.to_string(),
                &[("Order", ORDER_IOA)],
            );

            let system = ActorSystem::new("dst-crash-1");
            let mut state = ServerState::from_registry(system, registry);
            state.event_store = Some(store.clone());

            for action in &["AddItem", "SubmitOrder", "ConfirmOrder"] {
                let r = state
                    .dispatch_tenant_action(
                        &tenant,
                        "Order",
                        &eid,
                        action,
                        serde_json::json!({}),
                        &agent,
                    )
                    .await
                    .unwrap_or_else(|e| panic!("seed {seed}: {action} failed: {e}"));
                assert!(r.success, "seed {seed}: {action} failed: {:?}", r.error);
            }
        }
        // ServerState dropped — simulates crash.

        // Phase 2: Respawn with same SimEventStore.
        {
            let (_guard2, _clock2, _id_gen2) = install_deterministic_context(seed + 10000);
            let mut registry = SpecRegistry::new();
            let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
            registry.register_tenant(
                "default",
                csdl,
                CSDL_XML.to_string(),
                &[("Order", ORDER_IOA)],
            );

            let system = ActorSystem::new("dst-crash-2");
            let mut state = ServerState::from_registry(system, registry);
            state.event_store = Some(store.clone());

            // Continue from Confirmed → Processing.
            let r = state
                .dispatch_tenant_action(
                    &tenant,
                    "Order",
                    &eid,
                    "ProcessOrder",
                    serde_json::json!({}),
                    &agent,
                )
                .await
                .unwrap_or_else(|e| panic!("seed {seed}: ProcessOrder post-recovery failed: {e}"));
            assert!(
                r.success,
                "seed {seed}: ProcessOrder after recovery failed: {:?}",
                r.error
            );
            assert_eq!(r.state.status, "Processing");
        }
    }
}

// =========================================================================
// Test: Persistence stores correct number of events
// =========================================================================

#[tokio::test]
async fn dst_lifecycle_event_count() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let (state, sim_store) = build_state_with_sim(42);
    let tenant = TenantId::default();
    let agent = AgentContext::default();

    for action in &["AddItem", "SubmitOrder", "ConfirmOrder"] {
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                "o-count",
                action,
                serde_json::json!({}),
                &agent,
            )
            .await
            .unwrap();
    }

    // The SimEventStore should have events for this entity.
    // persistence_id format: "tenant:EntityType:EntityId"
    let events = sim_store.dump_journal("default:Order:o-count");
    // At minimum: Created (bootstrap) + AddItem + SubmitOrder + ConfirmOrder = 4
    assert!(
        events.len() >= 4,
        "Expected at least 4 events, got {}",
        events.len()
    );
}
