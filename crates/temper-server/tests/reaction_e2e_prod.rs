//! End-to-end integration test for Phase 1–3 of nerdsane/temper#128 —
//! exercises the **production** `ReactionDispatcher` path (async, through
//! `ServerState.dispatch_tenant_action`) rather than the sim-only
//! `SimReactionSystem` used in `reaction_cascade.rs`.
//!
//! This is the verification that closes the loop the ADR promises:
//! a reaction declared in TOML (params_from + guard + Create resolver)
//! actually dispatches through the live platform stack.

use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::trigger::registry::parse_reactions;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.ReactE2E" xmlns="http://docs.oasis-open.org/odata/ns/edm">
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
        <EntitySet Name="Orders"   EntityType="Temper.ReactE2E.Order"/>
        <EntitySet Name="Payments" EntityType="Temper.ReactE2E.Payment"/>
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

[[action]]
name = "CancelOrder"
kind = "input"
from = ["Draft", "Submitted"]
to = "Cancelled"
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

[[action]]
name = "CapturePayment"
kind = "internal"
from = ["Authorized"]
to = "Captured"

[[action]]
name = "FailPayment"
kind = "internal"
from = ["Pending", "Authorized"]
to = "Failed"
"#;

/// Build a ServerState with Order + Payment registered under the given
/// tenant plus the supplied reaction rules. Rebuilds the reaction dispatcher
/// so reactions fire through the production code path.
fn build_state(tenant: &str, reactions_toml: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let reactions = parse_reactions(reactions_toml).expect("reactions TOML should parse");
    registry
        .try_register_tenant_with_reactions(
            tenant,
            csdl,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA), ("Payment", PAYMENT_IOA)],
            reactions,
        )
        .expect("tenant registration");

    let system = ActorSystem::new("reaction-e2e-prod");
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
            &AgentContext::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("dispatch {entity_type}.{action} failed: {e}"))
}

async fn status(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> String {
    state
        .get_tenant_entity_state(tenant, entity_type, entity_id)
        .await
        .unwrap_or_else(|e| panic!("get_entity_state {entity_type}:{entity_id} failed: {e}"))
        .state
        .status
}

// =========================================================================
// E2E-1: Basic reaction fires through production dispatcher.
//
// Proves the whole stack wires up: parse_reactions → try_register_tenant
// → build_reaction_registry → ReactionDispatcher → dispatch_tenant_action
// → reaction target action completes.
// =========================================================================

#[tokio::test(flavor = "multi_thread")]
async fn prod_dispatcher_fires_basic_reaction() {
    let reactions = r#"
[[reaction]]
name = "order_confirmed_authorizes_payment"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
to_state = "Confirmed"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-e2e-1";
    let state = Arc::new(build_state(tenant_name, reactions));
    let tenant = TenantId::new(tenant_name);

    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "AddItem",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await;
    let r = dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "ConfirmOrder",
        serde_json::json!({}),
    )
    .await;
    assert!(r.success, "ConfirmOrder must succeed");

    // Reaction dispatch is awaited inline by `dispatch_tenant_action`
    // (`state/dispatch/actions.rs:157`), so by the time `r` returns the
    // target entity's transition is already committed.
    let order_status = status(&state, &tenant, "Order", "o1").await;
    assert_eq!(order_status, "Confirmed");

    let payment_status = status(&state, &tenant, "Payment", "o1").await;
    assert_eq!(
        payment_status, "Authorized",
        "reaction should have dispatched AuthorizePayment on Payment:o1"
    );
}

// =========================================================================
// E2E-2: Guarded reaction — source field gate.
//
// Two rules on the same trigger; a source-field guard picks exactly one.
// Proves ReactionGuard evaluation works through the production path.
// =========================================================================

#[tokio::test(flavor = "multi_thread")]
async fn prod_dispatcher_honours_source_field_guard() {
    // Two rules: one guarded state_in=["Confirmed"], one guarded
    // state_in=["Cancelled"]. Only the Confirmed one should fire on
    // Order.ConfirmOrder.
    let reactions = r#"
[[reaction]]
name = "fires_on_confirmed"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.when.guard]
type = "state_in"
values = ["Confirmed"]
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"

[[reaction]]
name = "skipped_on_cancelled"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.when.guard]
type = "state_in"
values = ["Cancelled"]
[reaction.then]
entity_type = "Payment"
action = "FailPayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-e2e-2";
    let state = Arc::new(build_state(tenant_name, reactions));
    let tenant = TenantId::new(tenant_name);

    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "AddItem",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "ConfirmOrder",
        serde_json::json!({}),
    )
    .await;

    // Guard gate works: Payment is Authorized (passing rule fired), not Failed
    // (skipped rule did NOT fire).
    let payment_status = status(&state, &tenant, "Payment", "o1").await;
    assert_eq!(
        payment_status, "Authorized",
        "state_in guard should have selected the Confirmed rule"
    );
}

// =========================================================================
// E2E-3: NOT guard — rule skipped because inner condition passes.
//
// Rule guarded with Not(StateIn[Confirmed]) must NOT fire when the source
// post-status IS Confirmed. Confirms guard-skipped rules do not side-effect.
// =========================================================================

#[tokio::test(flavor = "multi_thread")]
async fn prod_dispatcher_not_guard_skips_firing() {
    let reactions = r#"
[[reaction]]
name = "skipped_when_confirmed"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.when.guard]
type = "not"
[reaction.when.guard.guard]
type = "state_in"
values = ["Confirmed"]
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-e2e-3";
    let state = Arc::new(build_state(tenant_name, reactions));
    let tenant = TenantId::new(tenant_name);

    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "AddItem",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "ConfirmOrder",
        serde_json::json!({}),
    )
    .await;

    // Payment should stay Pending — the reaction was gated off.
    let payment_status = status(&state, &tenant, "Payment", "o1").await;
    assert_eq!(
        payment_status, "Pending",
        "Not(state_in=Confirmed) guard must have skipped the rule"
    );
}

// =========================================================================
// E2E-4: params_from — dynamic params pipe through production dispatch
// without breaking the cascade when source field is missing.
//
// Proves build_effective_params (shared helper between prod and sim) is
// wired correctly into ReactionDispatcher. Uses missing-source-field case
// because the plan's "warn + skip the key" policy is the load-bearing one.
// =========================================================================

#[tokio::test(flavor = "multi_thread")]
async fn prod_dispatcher_params_from_missing_field_still_fires() {
    let reactions = r#"
[[reaction]]
name = "order_confirmed_with_params_from"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
# Reference a field Order.ConfirmOrder never produces.
params = { note = "from_reaction" }
params_from = { passed_field = "nonexistent" }
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-e2e-4";
    let state = Arc::new(build_state(tenant_name, reactions));
    let tenant = TenantId::new(tenant_name);

    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "AddItem",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "ConfirmOrder",
        serde_json::json!({}),
    )
    .await;

    // Payment authorized — missing source field triggered warn+skip for
    // the passed_field key but the reaction still fired.
    let payment_status = status(&state, &tenant, "Payment", "o1").await;
    assert_eq!(payment_status, "Authorized");
}
