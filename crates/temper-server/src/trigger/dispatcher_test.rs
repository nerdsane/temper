use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::storage::StorageStack;

use super::dispatcher::reaction_authz_resource;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.ReactionAuthTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Target">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
        <Property Name="OwnerId" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Owner">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Targets" EntityType="Temper.ReactionAuthTest.Target"/>
        <EntitySet Name="Owners" EntityType="Temper.ReactionAuthTest.Owner"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const TARGET_IOA: &str = r#"
[automaton]
name = "Target"
states = ["Draft", "Active"]
initial = "Draft"

[[context_entity]]
name = "owner"
entity_type = "Owner"
id_field = "OwnerId"

[[action]]
name = "Create"
kind = "input"
from = ["Draft"]
to = "Active"
params = ["Name", "OwnerId"]
"#;

const OWNER_IOA: &str = r#"
[automaton]
name = "Owner"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]
"#;

fn state_with_store() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant_with_reactions(
            "default",
            parse_csdl(CSDL).expect("test CSDL should parse"),
            CSDL.to_string(),
            &[("Target", TARGET_IOA), ("Owner", OWNER_IOA)],
            Vec::new(),
        )
        .expect("test specs should register");
    let mut state = ServerState::from_registry(ActorSystem::new("reaction-auth-test"), registry);
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(71), None));
    state
}

#[tokio::test]
async fn reaction_create_cannot_spoof_trusted_resource_attributes() {
    let state = state_with_store();
    let tenant = TenantId::default();
    let context_entities = state
        .registry
        .read()
        .expect("registry lock should be healthy")
        .get_spec(&tenant, "Target")
        .expect("target spec should be registered")
        .automaton
        .context_entities
        .clone();
    assert_eq!(context_entities.len(), 1);
    assert_eq!(context_entities[0].name, "owner");
    let owner = state
        .get_or_create_tenant_entity(&tenant, "Owner", "owner-real-id", json!({}))
        .await
        .expect("context owner should be durable");
    assert_eq!(owner.state.status, "Active");
    assert_eq!(
        state
            .resolve_entity_status(&tenant, "Owner", "owner-real-id")
            .await
            .expect("context owner status should resolve durably"),
        Some("Active".to_string())
    );

    let (attrs, status) = reaction_authz_resource(
        &state,
        &tenant,
        "Target",
        "target-real-id",
        "Create",
        &json!({
            "Name": "reaction target",
            "OwnerId": "owner-real-id",
            "id": "attacker-selected-id",
            "status": "Active",
            "Id": "attacker-selected-Id",
            "Status": "Deleted",
            "ctx_owner_status": "Privileged",
            "has_spec": true
        }),
    )
    .await
    .expect("durably absent Create target should build initial authorization attrs");

    assert_eq!(attrs.get("id"), Some(&json!("target-real-id")));
    assert_eq!(attrs.get("status"), Some(&json!("Draft")));
    assert_eq!(attrs.get("Id"), Some(&json!("target-real-id")));
    assert_eq!(attrs.get("Status"), Some(&json!("Draft")));
    assert_eq!(attrs.get("has_spec"), Some(&json!(true)));
    assert_eq!(attrs.get("Name"), Some(&json!("reaction target")));
    assert_eq!(attrs.get("ctx_owner_status"), Some(&json!("Active")));
    assert_eq!(status, "Draft");

    state
        .authz
        .reload_policies(
            r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Target
                ) when {
                  resource.Id == "target-real-id" &&
                  resource.Status == "Draft" &&
                  resource.ctx_owner_status == "Active"
                };
                "#,
        )
        .expect("reaction policy should load");
    let security_ctx = AgentContext::for_service("reaction-auth-test")
        .security_ctx
        .expect("service context should carry a principal");
    state
        .authorize_with_context(&security_ctx, "Create", "Target", &attrs, tenant.as_str())
        .expect("trusted reaction attributes should satisfy the policy");
}
