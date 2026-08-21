//! End-to-end integration test for Phase 1–3 of nerdsane/temper#128 —
//! exercises the **production** `ReactionDispatcher` path (async, through
//! `ServerState.dispatch_tenant_action`) rather than the sim-only
//! `SimReactionSystem` used in `reaction_cascade.rs`.
//!
//! This is the verification that closes the loop the ADR promises:
//! a reaction declared in TOML (params_from + guard + Create resolver)
//! actually dispatches through the live platform stack.
#![allow(dead_code, unused_imports)]

pub use std::sync::Arc;

pub use temper_runtime::ActorSystem;
pub use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
pub use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
pub use temper_runtime::tenant::TenantId;
pub use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
pub use temper_server::request_context::AgentContext;
pub use temper_server::storage::{BoxedEventStore, StorageStack};
pub use temper_server::trigger::delivery::{
    PersistedReactionIntent, ReactionDeliveryRecord, ReactionDeliveryStatus,
    append_delivery_record, attach_intents, delivery_journal_id, extract_intents, extract_receipt,
    find_delivery_record, initialize_delivery_record, load_delivery_record, stable_delivery_id,
};
pub use temper_server::trigger::registry::parse_reactions;
use temper_spec::csdl::parse_csdl;
pub use temper_store_sim::SimEventStore;

pub const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
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

pub const ORDER_IOA: &str = r#"
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

pub const PAYMENT_IOA: &str = r#"
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
pub fn build_state(tenant: &str, reactions_toml: &str) -> ServerState {
    let mut state = build_state_without_storage(tenant, reactions_toml);
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(413), None));
    state
}

pub fn build_state_without_storage(tenant: &str, reactions_toml: &str) -> ServerState {
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
    state
        .authz
        .reload_tenant_policies(tenant, "permit(principal, action, resource);")
        .expect("reaction fixture policy should parse");
    state.rebuild_reaction_dispatcher();
    state
}

pub fn build_durable_state(tenant: &str, reactions_toml: &str) -> (ServerState, SimEventStore) {
    let store = SimEventStore::no_faults(414);
    let mut state = build_state(tenant, reactions_toml);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    (state, store)
}

pub async fn dispatch(
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

pub async fn status(
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
