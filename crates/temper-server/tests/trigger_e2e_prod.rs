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
use temper_server::state::DispatchExtOptions;
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

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

const FILE_WORKSPACE_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.TriggerFsE2E" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="File">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="workspace_id" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Workspace">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="used_bytes" Type="Edm.Int64"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Files" EntityType="Temper.TriggerFsE2E.File"/>
        <EntitySet Name="Workspaces" EntityType="Temper.TriggerFsE2E.Workspace"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const FILE_IOA: &str = r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[state]]
name = "workspace_id"
type = "string"
initial = ""

[[state]]
name = "size_bytes"
type = "counter"
initial = "0"

[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Created", "Ready"]
to = "Ready"
params = ["size_bytes"]

[[action.triggers]]
name = "file_stream_updated_increments_workspace_usage"
kind = "entity"
principal = "file-service"
target_entity = "Workspace"
target_action = "IncrementUsage"

[action.triggers.params_from]
size_bytes = "size_bytes"

[action.triggers.resolve_target]
type = "field"
field = "workspace_id"
"#;

const WORKSPACE_IOA: &str = r#"
[automaton]
name = "Workspace"
states = ["Active"]
initial = "Active"

[[state]]
name = "used_bytes"
type = "counter"
initial = "0"

[[action]]
name = "IncrementUsage"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{ type = "increment", var = "used_bytes", amount = "size_bytes" }]
params = ["size_bytes"]
"#;

/// Build a ServerState with Order + Payment registered under the tenant.
/// No `reactions.toml` — cross-entity wiring is declared inline on
/// `Order.ConfirmOrder` as an `[[action.triggers]]` block.
fn build_state(tenant: &str, tenant_policy: &str) -> ServerState {
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
    let mut state = ServerState::from_registry(system, registry);
    state
        .authz
        .reload_tenant_policies(tenant, tenant_policy)
        .expect("tenant policy should load");
    state.rebuild_reaction_dispatcher();
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(421), None));
    state
}

fn build_file_workspace_state(tenant: &str, tenant_policy: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(FILE_WORKSPACE_CSDL_XML).expect("filesystem CSDL should parse");
    registry
        .try_register_tenant_with_reactions(
            tenant,
            csdl,
            FILE_WORKSPACE_CSDL_XML.to_string(),
            &[("File", FILE_IOA), ("Workspace", WORKSPACE_IOA)],
            Vec::new(),
        )
        .expect("tenant registration should succeed with inline triggers");

    let system = ActorSystem::new("trigger-e2e-fs");
    let mut state = ServerState::from_registry(system, registry);
    state
        .authz
        .reload_tenant_policies(tenant, tenant_policy)
        .expect("tenant policy should load");
    state.rebuild_reaction_dispatcher();
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(422), None));
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
    let state = build_state(
        "trigger-e2e",
        r#"
permit(
    principal is Agent,
    action == Action::"AuthorizePayment",
    resource is Payment
) when {
    principal.agent_type == "payment-service"
};
"#,
    );

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

#[tokio::test]
async fn inline_action_triggers_respect_tenant_cedar_denials() {
    let tenant = TenantId::new("trigger-e2e-deny");
    let state = build_state("trigger-e2e-deny", "");

    let pay_id = "pay-denied";
    let order_id = "order-denied";
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

    let error = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            order_id,
            "ConfirmOrder",
            serde_json::json!({}),
            &AgentContext::system(),
        )
        .await
        .expect_err("awaited denied reaction must be reported");
    assert!(
        error.contains("terminal status Rejected"),
        "denial must surface its durable terminal outcome: {error}"
    );
    assert_eq!(
        state
            .get_tenant_entity_state(&tenant, "Order", order_id)
            .await
            .expect("source remains readable")
            .state
            .status,
        "Confirmed",
        "target denial must not roll back the committed source"
    );

    tokio::task::yield_now().await;

    let pay_resp = state
        .get_tenant_entity_state(&tenant, "Payment", pay_id)
        .await
        .expect("payment should still exist after denied trigger");
    assert_eq!(
        pay_resp.state.status, "Pending",
        "denied inline trigger must not advance Payment"
    );
}

#[tokio::test]
async fn inline_action_triggers_resolve_lowercase_source_fields_for_target_lookup() {
    let tenant = TenantId::new("trigger-e2e-fs");
    let state = build_file_workspace_state(
        "trigger-e2e-fs",
        r#"
permit(
    principal is Agent,
    action == Action::"IncrementUsage",
    resource is Workspace
) when {
    principal.agent_type == "file-service"
};
"#,
    );

    let workspace_id = "ws-1";
    let file_id = "file-1";

    let workspace = state
        .get_or_create_tenant_entity(&tenant, "Workspace", workspace_id, serde_json::json!({}))
        .await
        .expect("workspace seed should succeed");
    assert_eq!(workspace.state.status, "Active");

    let file = state
        .get_or_create_tenant_entity(
            &tenant,
            "File",
            file_id,
            serde_json::json!({ "workspace_id": workspace_id }),
        )
        .await
        .expect("file seed should succeed");
    assert_eq!(file.state.status, "Created");

    let resp = dispatch(
        &state,
        &tenant,
        "File",
        file_id,
        "StreamUpdated",
        serde_json::json!({ "size_bytes": 42 }),
    )
    .await;
    assert!(resp.success, "File.StreamUpdated should succeed");
    assert_eq!(resp.state.status, "Ready");

    tokio::task::yield_now().await;

    let workspace_after = state
        .get_tenant_entity_state(&tenant, "Workspace", workspace_id)
        .await
        .expect("workspace should exist after trigger fired");
    assert_eq!(
        workspace_after.state.fields["used_bytes"],
        serde_json::json!(42),
        "inline trigger should resolve workspace_id and increment workspace usage",
    );
}

#[tokio::test]
async fn inline_action_triggers_can_run_in_background_when_requested() {
    let tenant = TenantId::new("trigger-e2e-bg");
    let state = build_file_workspace_state(
        "trigger-e2e-bg",
        r#"
permit(
    principal is Agent,
    action == Action::"IncrementUsage",
    resource is Workspace
) when {
    principal.agent_type == "file-service"
};
"#,
    );

    let workspace_id = "ws-bg";
    let file_id = "file-bg";
    state
        .get_or_create_tenant_entity(&tenant, "Workspace", workspace_id, serde_json::json!({}))
        .await
        .expect("workspace seed should succeed");
    state
        .get_or_create_tenant_entity(
            &tenant,
            "File",
            file_id,
            serde_json::json!({ "workspace_id": workspace_id }),
        )
        .await
        .expect("file seed should succeed");

    let agent_ctx = AgentContext::system();
    let resp = state
        .dispatch_tenant_action_ext_typed(
            &tenant,
            "File",
            file_id,
            "StreamUpdated",
            serde_json::json!({ "size_bytes": 42 }),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: false,
                await_reactions: false,
            },
        )
        .await
        .expect("File.StreamUpdated should dispatch");

    assert!(resp.success, "source File transition should commit");
    assert_eq!(resp.state.status, "Ready");

    let mut observed_usage = None;
    for _ in 0..50 {
        let workspace_after = state
            .get_tenant_entity_state(&tenant, "Workspace", workspace_id)
            .await
            .expect("workspace should exist after trigger fired");
        if workspace_after.state.fields["used_bytes"] == serde_json::json!(42) {
            observed_usage = Some(workspace_after.state.fields["used_bytes"].clone());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(
        observed_usage,
        Some(serde_json::json!(42)),
        "background inline trigger should eventually increment workspace usage",
    );
}
