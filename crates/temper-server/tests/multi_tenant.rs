//! Multi-tenant integration test.
//!
//! Registers both ecommerce (Order) and linear (Issue) specs on the same
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
const ISSUE_IOA: &str = include_str!("../../../reference/linear/specs/issue.ioa.toml");

fn build_multi_tenant_state() -> ServerState {
    let mut registry = SpecRegistry::new();

    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "ecommerce",
        csdl.clone(),
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );

    let csdl2 = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "linear",
        csdl2,
        CSDL_XML.to_string(),
        &[("Issue", ISSUE_IOA)],
    );

    let system = ActorSystem::new("multi-tenant-test");
    ServerState::from_registry(system, registry)
}

// =========================================================================
// Registry isolation tests
// =========================================================================

#[test]
fn registry_ecommerce_has_order() {
    let state = build_multi_tenant_state();
    let ecom = TenantId::new("ecommerce");
    assert!(state.registry.get_table(&ecom, "Order").is_some());
}

#[test]
fn registry_linear_has_issue() {
    let state = build_multi_tenant_state();
    let linear = TenantId::new("linear");
    assert!(state.registry.get_table(&linear, "Issue").is_some());
}

#[test]
fn registry_ecommerce_does_not_have_issue() {
    let state = build_multi_tenant_state();
    let ecom = TenantId::new("ecommerce");
    assert!(state.registry.get_table(&ecom, "Issue").is_none());
}

#[test]
fn registry_linear_does_not_have_order() {
    let state = build_multi_tenant_state();
    let linear = TenantId::new("linear");
    assert!(state.registry.get_table(&linear, "Order").is_none());
}

// =========================================================================
// Actor dispatch isolation tests
// =========================================================================

#[tokio::test]
async fn tenant_actors_are_isolated() {
    let state = build_multi_tenant_state();
    let ecom = TenantId::new("ecommerce");
    let linear = TenantId::new("linear");

    // Spawn an Order actor for ecommerce tenant
    let order_state = state
        .get_tenant_entity_state(&ecom, "Order", "order-1")
        .await
        .expect("should spawn ecommerce Order actor");
    assert_eq!(order_state.state.status, "Draft");

    // Spawn an Issue actor for linear tenant
    let issue_state = state
        .get_tenant_entity_state(&linear, "Issue", "ISS-1")
        .await
        .expect("should spawn linear Issue actor");
    assert_eq!(issue_state.state.status, "Backlog");

    // Verify ecommerce can't access linear entities (no Issue table)
    let err = state
        .get_tenant_entity_state(&ecom, "Issue", "ISS-1")
        .await;
    assert!(err.is_err(), "ecommerce should not have Issue entities");
}

#[tokio::test]
async fn actions_on_one_tenant_dont_affect_another() {
    let state = build_multi_tenant_state();
    let ecom = TenantId::new("ecommerce");
    let linear = TenantId::new("linear");

    // Create an Order in ecommerce
    let _ = state
        .get_tenant_entity_state(&ecom, "Order", "shared-id")
        .await
        .unwrap();

    // Create an Issue in linear with the SAME entity_id
    let _ = state
        .get_tenant_entity_state(&linear, "Issue", "shared-id")
        .await
        .unwrap();

    // Mutate the ecommerce Order
    let result = state
        .dispatch_tenant_action(
            &ecom,
            "Order",
            "shared-id",
            "CancelOrder",
            serde_json::json!({"Reason": "changed mind"}),
        )
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.state.status, "Cancelled");

    // The linear Issue with the same ID should be unaffected
    let issue = state
        .get_tenant_entity_state(&linear, "Issue", "shared-id")
        .await
        .unwrap();
    assert_eq!(issue.state.status, "Backlog", "linear Issue should be unaffected by ecommerce action");
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

    // Ecommerce Order table works
    let order_table = state
        .registry
        .get_table(&TenantId::new("ecommerce"), "Order")
        .unwrap();
    assert_eq!(order_table.initial_state, "Draft");
    let r = order_table.evaluate("Draft", 1, "SubmitOrder");
    assert!(r.is_some());
    assert!(r.unwrap().success);

    // Linear Issue table works
    let issue_table = state
        .registry
        .get_table(&TenantId::new("linear"), "Issue")
        .unwrap();
    assert_eq!(issue_table.initial_state, "Backlog");
}
