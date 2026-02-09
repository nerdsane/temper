//! Multi-tenant integration test.
//!
//! Registers two tenants (alpha with Order, beta with Task) on the same
//! server and verifies that:
//! - Each tenant gets its own transition table
//! - Entity actors are isolated by tenant
//! - Actions on one tenant don't affect the other
//! - The SpecRegistry correctly routes lookups

use temper_runtime::tenant::TenantId;
use temper_runtime::ActorSystem;
use temper_server::registry::SpecRegistry;
use temper_server::ServerState;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

/// Minimal Task spec — inline to avoid external dependencies.
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

fn build_multi_tenant_state() -> ServerState {
    let mut registry = SpecRegistry::new();

    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "alpha",
        csdl.clone(),
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );

    let csdl2 = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "beta",
        csdl2,
        CSDL_XML.to_string(),
        &[("Task", TASK_IOA)],
    );

    let system = ActorSystem::new("multi-tenant-test");
    ServerState::from_registry(system, registry)
}

// =========================================================================
// Registry isolation tests
// =========================================================================

#[test]
fn registry_alpha_has_order() {
    let state = build_multi_tenant_state();
    let alpha = TenantId::new("alpha");
    assert!(state.registry.read().unwrap().get_table(&alpha, "Order").is_some());
}

#[test]
fn registry_beta_has_task() {
    let state = build_multi_tenant_state();
    let beta = TenantId::new("beta");
    assert!(state.registry.read().unwrap().get_table(&beta, "Task").is_some());
}

#[test]
fn registry_alpha_does_not_have_task() {
    let state = build_multi_tenant_state();
    let alpha = TenantId::new("alpha");
    assert!(state.registry.read().unwrap().get_table(&alpha, "Task").is_none());
}

#[test]
fn registry_beta_does_not_have_order() {
    let state = build_multi_tenant_state();
    let beta = TenantId::new("beta");
    assert!(state.registry.read().unwrap().get_table(&beta, "Order").is_none());
}

// =========================================================================
// Actor dispatch isolation tests
// =========================================================================

#[tokio::test]
async fn tenant_actors_are_isolated() {
    let state = build_multi_tenant_state();
    let alpha = TenantId::new("alpha");
    let beta = TenantId::new("beta");

    // Spawn an Order actor for alpha tenant
    let order_state = state
        .get_tenant_entity_state(&alpha, "Order", "order-1")
        .await
        .expect("should spawn alpha Order actor");
    assert_eq!(order_state.state.status, "Draft");

    // Spawn a Task actor for beta tenant
    let task_state = state
        .get_tenant_entity_state(&beta, "Task", "T-1")
        .await
        .expect("should spawn beta Task actor");
    assert_eq!(task_state.state.status, "Backlog");

    // Verify alpha can't access beta entities (no Task table)
    let err = state
        .get_tenant_entity_state(&alpha, "Task", "T-1")
        .await;
    assert!(err.is_err(), "alpha should not have Task entities");
}

#[tokio::test]
async fn actions_on_one_tenant_dont_affect_another() {
    let state = build_multi_tenant_state();
    let alpha = TenantId::new("alpha");
    let beta = TenantId::new("beta");

    // Create an Order in alpha
    let _ = state
        .get_tenant_entity_state(&alpha, "Order", "shared-id")
        .await
        .unwrap();

    // Create a Task in beta with the SAME entity_id
    let _ = state
        .get_tenant_entity_state(&beta, "Task", "shared-id")
        .await
        .unwrap();

    // Mutate the alpha Order
    let result = state
        .dispatch_tenant_action(
            &alpha,
            "Order",
            "shared-id",
            "CancelOrder",
            serde_json::json!({"Reason": "changed mind"}),
        )
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.state.status, "Cancelled");

    // The beta Task with the same ID should be unaffected
    let task = state
        .get_tenant_entity_state(&beta, "Task", "shared-id")
        .await
        .unwrap();
    assert_eq!(task.state.status, "Backlog", "beta Task should be unaffected by alpha action");
}

#[tokio::test]
async fn same_entity_type_different_tenants() {
    let mut registry = SpecRegistry::new();

    // Register Order in TWO different tenants
    let csdl1 = parse_csdl(CSDL_XML).unwrap();
    let csdl2 = parse_csdl(CSDL_XML).unwrap();
    registry.register_tenant("tenant-a", csdl1, CSDL_XML.to_string(), &[("Order", ORDER_IOA)]);
    registry.register_tenant("tenant-b", csdl2, CSDL_XML.to_string(), &[("Order", ORDER_IOA)]);

    let system = ActorSystem::new("dual-tenant");
    let state = ServerState::from_registry(system, registry);

    let a = TenantId::new("tenant-a");
    let b = TenantId::new("tenant-b");

    // Create Order #1 in tenant-a and cancel it
    let _ = state.get_tenant_entity_state(&a, "Order", "o1").await.unwrap();
    let r = state
        .dispatch_tenant_action(&a, "Order", "o1", "CancelOrder", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(r.state.status, "Cancelled");

    // Create Order #1 in tenant-b — should be independent, still in Draft
    let r = state.get_tenant_entity_state(&b, "Order", "o1").await.unwrap();
    assert_eq!(r.state.status, "Draft", "tenant-b Order should be independent from tenant-a");
}

// =========================================================================
// Transition table correctness
// =========================================================================

#[test]
fn registry_tables_are_functional() {
    let state = build_multi_tenant_state();
    let registry = state.registry.read().unwrap();

    // Alpha Order table works
    let order_table = registry
        .get_table(&TenantId::new("alpha"), "Order")
        .unwrap();
    assert_eq!(order_table.initial_state, "Draft");
    let r = order_table.evaluate("Draft", 1, "SubmitOrder");
    assert!(r.is_some());
    assert!(r.unwrap().success);

    // Beta Task table works
    let task_table = registry
        .get_table(&TenantId::new("beta"), "Task")
        .unwrap();
    assert_eq!(task_table.initial_state, "Backlog");
}
