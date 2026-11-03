//! A reaction's guard reads a variable the source record stores no value
//! for as its declared initial value, as action guards do.
//!
//! The second version of Order declares `note` (initial "held") and two
//! reactions on ConfirmOrder: one guarded `note == 'held'` authorizes the
//! payment, one guarded `note != 'held'` fails it. A record made under the
//! first version stores no `note`, so only the declared value tells them
//! apart.

use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;

const SERVICE: &str = "declared-defaults-reactions";
const TENANT: &str = "declared-defaults";

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.DeclaredDefaults" xmlns="http://docs.oasis-open.org/odata/ns/edm">
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
        <EntitySet Name="Orders" EntityType="Temper.DeclaredDefaults.Order"/>
        <EntitySet Name="Payments" EntityType="Temper.DeclaredDefaults.Payment"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const ORDER_V1: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Confirmed"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"

[[action]]
name = "ConfirmOrder"
kind = "input"
from = ["Submitted"]
to = "Confirmed"
"#;

/// What the second version adds: `note`, and the two reactions on
/// ConfirmOrder.
const ADDED: &str = r#"
[[state]]
name = "note"
type = "string"
initial = "held"
"#;

const REACTIONS: &str = r#"
[[action.triggers]]
name = "held_authorizes"
kind = "entity"
guard = "note == 'held'"
target_entity = "Payment"
target_action = "AuthorizePayment"
resolve_target = { kind = "same_id" }

[[action.triggers]]
name = "otherwise_fails"
kind = "entity"
guard = "note != 'held'"
target_entity = "Payment"
target_action = "FailPayment"
resolve_target = { kind = "same_id" }
"#;

const PAYMENT: &str = r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized", "Failed"]
initial = "Pending"

[[action]]
name = "AuthorizePayment"
kind = "internal"
from = ["Pending"]
to = "Authorized"

[[action]]
name = "FailPayment"
kind = "internal"
from = ["Pending"]
to = "Failed"
"#;

fn order_v2() -> String {
    let confirm = "to = \"Confirmed\"\n";
    assert_eq!(ORDER_V1.matches(confirm).count(), 1);
    ORDER_V1.replace(confirm, &format!("{confirm}{REACTIONS}")) + ADDED
}

fn install(state: &ServerState, order: &str) {
    state
        .registry
        .write()
        .expect("registry lock") // ci-ok: infallible lock
        .try_register_tenant(
            TENANT,
            parse_csdl(CSDL_XML).expect("CSDL parse"),
            CSDL_XML.to_string(),
            &[("Order", order), ("Payment", PAYMENT)],
        )
        .expect("tenant registration");
    state.rebuild_reaction_dispatcher();
}

fn build_state() -> ServerState {
    let state = ServerState::from_registry(ActorSystem::new(SERVICE), SpecRegistry::new());
    install(&state, ORDER_V1);
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"
            permit(principal, action, resource);
            "#,
        )
        .expect("policy parses");
    state
}

async fn act(state: &ServerState, entity_type: &str, id: &str, action: &str) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::new(TENANT),
            entity_type,
            id,
            action,
            serde_json::json!({}),
            &AgentContext::for_service(SERVICE),
        )
        .await
        .unwrap_or_else(|e| panic!("{entity_type}.{action} on {id}: {e}"));
    assert!(response.success, "{action} on {id}: {:?}", response.error);
}

async fn status(state: &ServerState, entity_type: &str, id: &str) -> String {
    state
        .get_tenant_entity_state(&TenantId::new(TENANT), entity_type, id)
        .await
        .unwrap_or_else(|e| panic!("{entity_type}:{id}: {e}"))
        .state
        .status
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_guard_reads_a_newly_declared_field_as_its_default() {
    let state = Arc::new(build_state());
    act(&state, "Order", "old", "SubmitOrder").await;

    install(&state, &order_v2());
    act(&state, "Order", "new", "SubmitOrder").await;

    for id in ["old", "new"] {
        act(&state, "Order", id, "ConfirmOrder").await;
        assert_eq!(status(&state, "Order", id).await, "Confirmed");
        assert_eq!(
            status(&state, "Payment", id).await,
            "Authorized",
            "Order {id} stores no note, which reads as 'held'"
        );
    }
}
