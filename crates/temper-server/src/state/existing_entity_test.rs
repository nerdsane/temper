//! A read that finds a deleted entity cleans up only the actor that answered.

use futures_util::poll;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::ServerState;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

fn order_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "tenant-a",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    ServerState::from_registry(ActorSystem::new("existing-entity"), registry)
}

/// The uid of the live actor registered for this order, if any.
fn live_actor_uid(state: &ServerState, id: &str) -> Option<uuid::Uuid> {
    let key = format!("tenant-a:Order:{id}");
    state
        .actor_registry
        .read()
        .unwrap()
        .get(&key)
        .filter(|actor| !actor.is_closed())
        .map(|actor| actor.id().uid)
}

async fn add_item(state: &ServerState, tenant: &TenantId, id: &str) {
    let result = state
        .dispatch_tenant_action(
            tenant,
            "Order",
            id,
            "AddItem",
            serde_json::json!({"ProductId": "p1"}),
            &AgentContext::default(),
        )
        .await;
    assert!(result.is_ok_and(|r| r.success), "AddItem failed");
}

#[tokio::test]
async fn a_read_answered_deleted_leaves_the_recreated_entity_registered() {
    let state = order_state();
    let tenant = TenantId::new("tenant-a");
    add_item(&state, &tenant, "o-1").await;

    // The delete reaches the actor first, then the read asks it for its
    // state, so the read's answer is the deletion.
    let mut delete = std::pin::pin!(state.delete_tenant_entity(&tenant, "Order", "o-1"));
    assert!(poll!(delete.as_mut()).is_pending());
    let mut read = std::pin::pin!(state.get_existing_entity_state(&tenant, "Order", "o-1"));
    assert!(poll!(read.as_mut()).is_pending());

    // Before the read resumes, the delete finishes and the id is used again.
    delete.await.expect("delete succeeds");
    add_item(&state, &tenant, "o-1").await;
    let recreated = live_actor_uid(&state, "o-1").expect("the new order has an actor");

    let stale = read.await.expect("the read answers");
    assert!(stale.is_none(), "the read saw the deletion");

    assert_eq!(
        live_actor_uid(&state, "o-1"),
        Some(recreated),
        "the stale read removed the new order's actor"
    );
    assert!(
        state.entity_exists(&tenant, "Order", "o-1"),
        "the stale read removed the new order from the index"
    );
    let current = state
        .get_existing_entity_state(&tenant, "Order", "o-1")
        .await
        .expect("the read answers")
        .expect("the new order exists");
    assert_eq!(current.state.status, "Draft");
}
