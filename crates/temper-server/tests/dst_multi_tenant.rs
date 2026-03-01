//! DST multi-tenant isolation tests.
//!
//! Verifies that tenant isolation is maintained under simulation:
//! - Entities are scoped to tenants
//! - Actions can't cross tenant boundaries
//! - Hot-swapping one tenant doesn't affect another

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

const TASK_IOA: &str = r#"
[automaton]
name = "Task"
initial = "Backlog"
states = ["Backlog", "InProgress", "Done", "Cancelled"]

[[action]]
name = "StartWork"
from = ["Backlog"]
to = "InProgress"
kind = "internal"

[[action]]
name = "Complete"
from = ["InProgress"]
to = "Done"
kind = "internal"

[[action]]
name = "Cancel"
from = ["Backlog", "InProgress"]
to = "Cancelled"
kind = "input"
"#;

fn build_two_tenant_state(seed: u64) -> ServerState {
    let sim_store = SimEventStore::no_faults(seed);
    let store = ServerEventStore::Sim(sim_store);

    let mut registry = SpecRegistry::new();
    let csdl_a = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "tenant-a",
        csdl_a,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );

    let csdl_b = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "tenant-b",
        csdl_b,
        CSDL_XML.to_string(),
        &[("Task", TASK_IOA)],
    );

    let system = ActorSystem::new("dst-mt");
    let mut state = ServerState::from_registry(system, registry);
    state.event_store = Some(Arc::new(store));
    state
}

// =========================================================================
// Test: Tenant A can only access its own entity types
// =========================================================================

#[tokio::test]
async fn dst_tenant_a_dispatches_order() {
    for seed in 0..50 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let state = build_two_tenant_state(seed);
        let agent = AgentContext::default();

        let r = state
            .dispatch_tenant_action(
                &TenantId::new("tenant-a"),
                "Order",
                &format!("o-{seed}"),
                "AddItem",
                serde_json::json!({}),
                &agent,
            )
            .await;
        assert!(r.is_ok(), "seed {seed}: tenant-a should be able to create Order");
    }
}

#[tokio::test]
async fn dst_tenant_a_cannot_dispatch_task() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let state = build_two_tenant_state(42);
    let agent = AgentContext::default();

    let r = state
        .dispatch_tenant_action(
            &TenantId::new("tenant-a"),
            "Task",
            "t-1",
            "StartWork",
            serde_json::json!({}),
            &agent,
        )
        .await;
    assert!(
        r.is_err(),
        "tenant-a should not be able to dispatch Task actions"
    );
}

// =========================================================================
// Test: Tenant B can only access its own entity types
// =========================================================================

#[tokio::test]
async fn dst_tenant_b_dispatches_task() {
    for seed in 0..50 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let state = build_two_tenant_state(seed);
        let agent = AgentContext::default();

        let r = state
            .dispatch_tenant_action(
                &TenantId::new("tenant-b"),
                "Task",
                &format!("t-{seed}"),
                "StartWork",
                serde_json::json!({}),
                &agent,
            )
            .await;
        assert!(r.is_ok(), "seed {seed}: tenant-b should be able to create Task");
    }
}

#[tokio::test]
async fn dst_tenant_b_cannot_dispatch_order() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let state = build_two_tenant_state(42);
    let agent = AgentContext::default();

    let r = state
        .dispatch_tenant_action(
            &TenantId::new("tenant-b"),
            "Order",
            "o-1",
            "AddItem",
            serde_json::json!({}),
            &agent,
        )
        .await;
    assert!(
        r.is_err(),
        "tenant-b should not be able to dispatch Order actions"
    );
}

// =========================================================================
// Test: Actions on one tenant don't affect another
// =========================================================================

#[tokio::test]
async fn dst_tenant_isolation_under_load() {
    for seed in 0..50 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let state = build_two_tenant_state(seed);
        let agent = AgentContext::default();

        // Create and advance an Order in tenant-a.
        state
            .dispatch_tenant_action(
                &TenantId::new("tenant-a"),
                "Order",
                &format!("o-{seed}"),
                "AddItem",
                serde_json::json!({}),
                &agent,
            )
            .await
            .expect("AddItem");

        state
            .dispatch_tenant_action(
                &TenantId::new("tenant-a"),
                "Order",
                &format!("o-{seed}"),
                "SubmitOrder",
                serde_json::json!({}),
                &agent,
            )
            .await
            .expect("SubmitOrder");

        // Create and advance a Task in tenant-b.
        let task_r = state
            .dispatch_tenant_action(
                &TenantId::new("tenant-b"),
                "Task",
                &format!("t-{seed}"),
                "StartWork",
                serde_json::json!({}),
                &agent,
            )
            .await
            .expect("StartWork");
        assert_eq!(task_r.state.status, "InProgress");

        // Verify tenant-a's Order is still Submitted (not affected by tenant-b).
        let order_state = state
            .dispatch_tenant_action(
                &TenantId::new("tenant-a"),
                "Order",
                &format!("o-{seed}"),
                "ConfirmOrder",
                serde_json::json!({}),
                &agent,
            )
            .await
            .expect("ConfirmOrder");
        assert_eq!(
            order_state.state.status, "Confirmed",
            "seed {seed}: Order should have been Submitted->Confirmed"
        );
    }
}

// =========================================================================
// Test: Hot-swapping one tenant doesn't affect another
// =========================================================================

#[tokio::test]
async fn dst_hotswap_tenant_isolation() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let state = build_two_tenant_state(42);
    let agent = AgentContext::default();

    // Create a Task in tenant-b.
    state
        .dispatch_tenant_action(
            &TenantId::new("tenant-b"),
            "Task",
            "t-1",
            "StartWork",
            serde_json::json!({}),
            &agent,
        )
        .await
        .expect("StartWork");

    // Get tenant-b's table version before.
    let v_before = {
        let reg = state.registry.read().expect("registry lock"); // ci-ok: infallible lock
        let spec = reg.get_spec(&TenantId::new("tenant-b"), "Task").expect("Task spec");
        spec.swap_controller().version()
    };

    // Hot-swap tenant-a's Order spec.
    {
        let mut reg = state.registry.write().expect("registry lock"); // ci-ok: infallible lock
        let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
        reg.register_tenant(
            "tenant-a",
            csdl,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA)],
        );
    }

    // Tenant-b's table version should be unchanged.
    let v_after = {
        let reg = state.registry.read().expect("registry lock"); // ci-ok: infallible lock
        let spec = reg.get_spec(&TenantId::new("tenant-b"), "Task").expect("Task spec");
        spec.swap_controller().version()
    };

    assert_eq!(v_before, v_after, "tenant-b's table should not be affected by tenant-a hot-swap");

    // Tenant-b's Task should still be in InProgress.
    let r = state
        .dispatch_tenant_action(
            &TenantId::new("tenant-b"),
            "Task",
            "t-1",
            "Complete",
            serde_json::json!({}),
            &agent,
        )
        .await
        .expect("Complete");
    assert_eq!(r.state.status, "Done");
}
