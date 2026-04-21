//! End-to-end integration test for ADR-0046 inline `[[action.triggers]]`.
//!
//! Parallel to `reaction_e2e_prod.rs`, but declares cross-entity wiring
//! inline on the source entity's action rather than in a separate
//! `reactions.toml`. Proves the full ADR-0046 chain works:
//!
//! spec → parse → validate → synthesize_action_trigger_reaction →
//! build_reaction_registry → ReactionDispatcher → target entity commits.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.TriggerE2E" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Order">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Payment">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Orders"   EntityType="Temper.TriggerE2E.Order"/>
        <EntitySet Name="Payments" EntityType="Temper.TriggerE2E.Payment"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const ORDER_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Confirmed", "Cancelled"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[state]]
name = "payment_id"
type = "string"
initial = ""

[[action]]
name = "AddItem"
kind = "input"
from = ["Draft"]

[[action]]
name = "SubmitOrder"
kind = "internal"
from = ["Draft"]
to = "Submitted"
guard = "items > 0"

[[action]]
name = "ConfirmOrder"
kind = "internal"
from = ["Submitted"]
to = "Confirmed"

# ADR-0046 inline trigger: fire Payment.AuthorizePayment post-commit.
[[action.triggers]]
name = "confirm_triggers_auth"
kind = "entity"
principal = "payment-service"
target_entity = "Payment"
target_action = "AuthorizePayment"

[action.triggers.resolve_target]
type = "field"
field = "payment_id"
"#;

const PAYMENT_IOA: &str = r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized", "Captured", "Failed"]
initial = "Pending"

[[action]]
name = "AuthorizePayment"
kind = "internal"
from = ["Pending"]
to = "Authorized"
"#;

/// Build a ServerState with Order + Payment registered under the tenant.
/// No `reactions.toml` — cross-entity wiring is declared inline on
/// `Order.ConfirmOrder` as an `[[action.triggers]]` block.
fn build_state(tenant: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry
        .try_register_tenant_with_reactions(
            tenant,
            csdl,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA), ("Payment", PAYMENT_IOA)],
            Vec::new(), // No external reactions.toml — triggers are inline.
        )
        .expect("tenant registration should succeed with inline triggers");

    let system = ActorSystem::new("trigger-e2e-prod");
    let state = ServerState::from_registry(system, registry);
    state.rebuild_reaction_dispatcher();
    state
}

async fn dispatch(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
) -> temper_server::entity_actor::EntityResponse {
    state
        .dispatch_tenant_action(
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            &AgentContext::system(),
        )
        .await
        .expect("dispatch should succeed")
}

#[tokio::test]
async fn inline_action_triggers_fire_through_production_dispatcher() {
    let tenant = TenantId::new("trigger-e2e");
    let state = build_state("trigger-e2e");

    // Seed a Payment entity we'll reference from the Order's trigger.
    // Payment starts in Pending.
    let pay_id = "pay-1";
    // Seed an Order, reference the Payment via payment_id, advance to Submitted.
    let order_id = "order-1";
    dispatch(
        &state,
        &tenant,
        "Order",
        order_id,
        "AddItem",
        serde_json::json!({ "payment_id": pay_id }),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        order_id,
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await;

    // Pre-condition: Payment is Pending.
    // (Payments are auto-created on first reference; default initial status.)
    // Dispatch ConfirmOrder — the inline trigger must fire AuthorizePayment.
    let resp = dispatch(
        &state,
        &tenant,
        "Order",
        order_id,
        "ConfirmOrder",
        serde_json::json!({}),
    )
    .await;
    assert!(resp.success, "Order.ConfirmOrder should succeed");
    assert_eq!(resp.state.status, "Confirmed");

    // Give the fire-and-forget reaction a moment to dispatch through the
    // event loop. In practice the reaction is awaited inside the dispatch
    // path, but in tests we read synchronously from the registry next.
    // A short yield suffices under tokio::test.
    tokio::task::yield_now().await;

    // Post-condition: Payment should now be Authorized.
    // Query the payment's current state via the server.
    let pay_resp = state
        .get_tenant_entity_state(&tenant, "Payment", pay_id)
        .await
        .expect("payment should exist after trigger fired");
    assert_eq!(
        pay_resp.state.status, "Authorized",
        "inline [[action.triggers]] must advance Payment to Authorized"
    );
}
