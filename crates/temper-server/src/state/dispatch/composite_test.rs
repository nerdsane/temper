use std::collections::BTreeMap;
#[cfg(feature = "sim")]
use std::sync::Arc;

use serde_json::json;
use temper_runtime::ActorSystem;
#[cfg(feature = "sim")]
use temper_runtime::persistence::EventStore;
use temper_spec::csdl::parse_csdl;
#[cfg(feature = "sim")]
use temper_store_sim::SimEventStore;
#[cfg(feature = "sim")]
use temper_store_turso::TursoEventStore;

use crate::request_context::AgentContext;
use crate::state::ServerState;
#[cfg(feature = "sim")]
use crate::storage::StorageStack;

use super::*;

#[test]
fn implicit_composite_idempotency_changes_with_integration_result() {
    let agent = AgentContext::for_service("composite-test");
    let first = composite_parent_idempotency(
        &agent,
        &json!({
            "sub_writes": [{
                "entity_type": "Ref",
                "entity_id": "rf-1",
                "action": "Create",
                "params": {"Name": "refs/heads/topic"}
            }]
        }),
    );
    let second = composite_parent_idempotency(
        &agent,
        &json!({
            "sub_writes": [{
                "entity_type": "Ref",
                "entity_id": "rf-1",
                "action": "Delete",
                "params": {}
            }]
        }),
    );

    assert_ne!(first, second);
}

#[test]
fn composite_idempotency_window_rejects_an_internal_sequence_gap() {
    let event = CompositeEvent {
        tenant: "default".to_string(),
        parent_entity_type: "Parent".to_string(),
        parent_entity_id: "gap".to_string(),
        parent_action: "CreateChild".to_string(),
        composite_idempotency_key: "gap-key".to_string(),
        sub_writes: Vec::new(),
    };
    let mut first = composite_event_envelope("default:Parent:gap", &event).unwrap();
    first.sequence_nr = 1;
    let mut third = first.clone();
    third.sequence_nr = 3;
    let mut later = first.clone();
    later.sequence_nr = 4;

    let error = validate_composite_idempotency_window(
        "default:Parent:gap",
        0,
        3,
        3,
        &[first, third, later],
    )
    .expect_err("a missing sequence must not be treated as a complete idempotency window");
    assert!(error.to_string().contains("expected sequence 2, found 3"));
}

#[test]
fn ingest_pack_generated_sub_writes_use_parent_composite_gate_only() {
    let metadata = CompositeActionMetadata {
        cedar_gate: Some(temper_jit::table::CompositeCedarGate {
            principal: "request.principal".to_string(),
            resource: "this".to_string(),
            action: "Repository::IngestPack".to_string(),
        }),
        record_parent_event: true,
        sub_writes: vec![
            temper_jit::table::SubWriteSpec {
                target_entity: "Blob".to_string(),
                action: "Create".to_string(),
                generated_from: Some("pack_bytes".to_string()),
            },
            temper_jit::table::SubWriteSpec {
                target_entity: "Ref".to_string(),
                action: "Delete".to_string(),
                generated_from: Some("ref_updates".to_string()),
            },
        ],
    };

    assert!(composite_sub_write_uses_parent_gate(
        &metadata, "Blob", "Create"
    ));
    assert!(composite_sub_write_uses_parent_gate(
        &metadata, "Ref", "Delete"
    ));
    assert!(!composite_sub_write_uses_parent_gate(
        &metadata,
        "Ref",
        "ForceUpdate"
    ));
    assert!(!composite_sub_write_uses_parent_gate(
        &CompositeActionMetadata {
            cedar_gate: None,
            ..metadata.clone()
        },
        "Blob",
        "Create"
    ));
}

const COMPOSITE_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.CompositeTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Parent">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Child">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Name" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="App">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="OwnerId" Type="Edm.String" Nullable="false"/>
        <Property Name="Name" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Blob">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="RepositoryId" Type="Edm.String" Nullable="false"/>
        <Property Name="CanonicalBytes" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Ref">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="RepositoryId" Type="Edm.String" Nullable="false"/>
        <Property Name="Name" Type="Edm.String" Nullable="false"/>
        <Property Name="TargetCommitSha" Type="Edm.String" Nullable="false"/>
        <Property Name="Kind" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Parents" EntityType="Temper.CompositeTest.Parent"/>
        <EntitySet Name="Children" EntityType="Temper.CompositeTest.Child"/>
        <EntitySet Name="Apps" EntityType="Temper.CompositeTest.App"/>
        <EntitySet Name="Blobs" EntityType="Temper.CompositeTest.Blob"/>
        <EntitySet Name="Refs" EntityType="Temper.CompositeTest.Ref"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const PARENT_IOA: &str = r#"
[automaton]
name = "Parent"
states = ["Active"]
initial = "Active"

[[action]]
name = "CreateChild"
kind = "Composite"
from = ["Active"]
to = "Active"
params = ["Reason"]

[[action.sub_writes]]
target_entity = "Child"
action = "Create"
generated_from = "child"

[[action.sub_writes]]
target_entity = "App"
action = "Create"
generated_from = "app_metadata"

[[action]]
name = "IngestPack"
kind = "Composite"
from = ["Active"]
to = "Active"
record_parent_event = false
params = ["Reason"]

[[action.cedar_gate]]
principal = "request.principal"
resource = "this"
action = "Repository::IngestPack"

[[action.sub_writes]]
target_entity = "Blob"
action = "Create"
generated_from = "pack_bytes"

[[action.sub_writes]]
target_entity = "Ref"
action = "Create"
generated_from = "ref_updates"

[[action.sub_writes]]
target_entity = "Ref"
action = "Update"
generated_from = "ref_updates"

[[action.sub_writes]]
target_entity = "Ref"
action = "Delete"
generated_from = "ref_updates"

[[action]]
name = "DeleteChild"
kind = "Composite"
from = ["Active"]
to = "Active"
params = ["ChildId"]

[[action.sub_writes]]
target_entity = "Child"
action = "Delete"
generated_from = "child"

[[action]]
name = "RenameChild"
kind = "Composite"
from = ["Active"]
to = "Active"
params = ["ChildId", "Name"]

[[action.sub_writes]]
target_entity = "Child"
action = "Rename"
generated_from = "child"

[[action]]
name = "CreateChildWithoutParentEvent"
kind = "Composite"
from = ["Active"]
to = "Active"
record_parent_event = false
params = ["Reason"]

[[action.sub_writes]]
target_entity = "Child"
action = "Create"
generated_from = "child"
"#;

const CHILD_IOA: &str = r#"
[automaton]
name = "Child"
states = ["Draft", "Active", "Deleted"]
initial = "Draft"

[[key]]
name = "name"
properties = ["Name"]

[[action]]
name = "Create"
kind = "input"
from = ["Draft"]
to = "Active"
params = ["Name"]

[[action]]
name = "Delete"
kind = "input"
from = ["Active"]
to = "Deleted"
params = []

[[action]]
name = "Rename"
kind = "input"
from = ["Active"]
to = "Active"
params = ["Name"]
"#;

const APP_IOA: &str = r#"
[automaton]
name = "App"
states = ["Active"]
initial = "Active"

[[action]]
name = "Create"
kind = "input"
from = ["Active"]
to = "Active"
params = ["OwnerId", "Name"]
"#;

const BLOB_IOA: &str = r#"
[automaton]
name = "Blob"
states = ["Durable"]
initial = "Durable"
allow_indefinite_states = ["Durable"]

[[state]]
name = "RepositoryId"
type = "string"
initial = ""

[[state]]
name = "CanonicalBytes"
type = "string"
initial = ""

[[action]]
name = "Create"
kind = "input"
from = ["Durable"]
params = ["RepositoryId", "CanonicalBytes"]
"#;

const REF_IOA: &str = r#"
[automaton]
name = "Ref"
states = ["Active", "Deleted"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "TargetCommitSha"
type = "string"
initial = ""

[[action]]
name = "Create"
kind = "input"
from = ["Active"]
to = "Active"
params = ["RepositoryId", "Name", "TargetCommitSha", "Kind"]

[[action]]
name = "Update"
kind = "input"
from = ["Active"]
to = "Active"
params = ["PreviousCommitSha", "NewCommitSha", "TargetCommitSha"]

[[action]]
name = "Delete"
kind = "input"
from = ["Active"]
to = "Deleted"
params = ["PreviousCommitSha"]
"#;

fn composite_test_state() -> ServerState {
    let csdl = parse_csdl(COMPOSITE_CSDL).expect("test CSDL should parse");
    let mut specs = BTreeMap::new();
    specs.insert("Parent".to_string(), PARENT_IOA.to_string());
    specs.insert("Child".to_string(), CHILD_IOA.to_string());
    specs.insert("App".to_string(), APP_IOA.to_string());
    specs.insert("Blob".to_string(), BLOB_IOA.to_string());
    specs.insert("Ref".to_string(), REF_IOA.to_string());
    ServerState::with_specs(
        ActorSystem::new("composite-dispatch-test"),
        csdl,
        COMPOSITE_CSDL.to_string(),
        specs,
    )
    .expect("test state should build")
}

#[cfg(feature = "sim")]
fn composite_test_state_with_store(store: SimEventStore) -> ServerState {
    let csdl = parse_csdl(COMPOSITE_CSDL).expect("test CSDL should parse");
    let mut specs = BTreeMap::new();
    specs.insert("Parent".to_string(), PARENT_IOA.to_string());
    specs.insert("Child".to_string(), CHILD_IOA.to_string());
    specs.insert("App".to_string(), APP_IOA.to_string());
    specs.insert("Blob".to_string(), BLOB_IOA.to_string());
    specs.insert("Ref".to_string(), REF_IOA.to_string());
    ServerState::with_storage_stack(
        ActorSystem::new("composite-dispatch-test"),
        csdl,
        COMPOSITE_CSDL.to_string(),
        specs,
        StorageStack::from_sim(store, None),
    )
    .expect("test state should build")
}

#[tokio::test]
async fn composite_action_rejects_caller_supplied_sub_writes() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let err = state
        .dispatch_tenant_action(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            json!({
                "Reason": "unit-test",
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": "child-1",
                    "action": "Create",
                    "params": { "Name": "created through composite" }
                }]
            }),
            &agent,
        )
        .await
        .expect_err("caller-supplied sub_writes should be rejected");

    assert!(
        err.contains("cannot accept caller-supplied sub_writes"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn composite_integration_result_executes_declared_sub_writes() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            json!({ "Reason": "unit-test" }),
            &agent,
        )
        .await
        .expect("composite parent action should run");

    assert!(response.success);
    assert_eq!(response.state.status, "Active");
    assert!(response.state.fields.get("sub_writes").is_none());

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": "child-1",
                    "action": "Create",
                    "params": { "Name": "created through composite integration" }
                }]
            }),
            &agent,
        )
        .await
        .expect("composite integration result should apply");

    assert!(applied);

    let child = state
        .get_tenant_entity_state(&tenant, "Child", "child-1")
        .await
        .expect("child state should be readable");
    assert_eq!(child.state.status, "Active");
    assert_eq!(
        child.state.fields.get("Name"),
        Some(&json!("created through composite integration"))
    );
}

#[tokio::test]
async fn composite_sub_write_authorization_receives_action_context() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    state
        .authz
        .reload_policies(
            r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Child
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Parent.CreateChild"
                };
                "#,
        )
        .expect("policy should load");

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-auth",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": "child-auth-ok",
                    "action": "Create",
                    "params": { "Name": "authorized through action_context" }
                }]
            }),
            &agent,
        )
        .await
        .expect("composite sub-write should be authorized by action_context");
    assert!(applied);

    state
        .authz
        .reload_policies(
            r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Child
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Other.Action"
                };
                "#,
        )
        .expect("policy should load");

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-auth",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": "child-auth-denied",
                    "action": "Create",
                    "params": { "Name": "should be denied" }
                }]
            }),
            &agent,
        )
        .await
        .expect_err("mismatched action_context should deny sub-write")
        .to_string();
    assert!(
        err.contains("sub-write 0 denied"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn composite_create_authorization_keeps_trusted_attributes_authoritative() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    state
        .authz
        .reload_policies(
            r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Child
                ) when {
                  resource.id == "child-trusted-auth" &&
                  resource.status == "Draft" &&
                  resource.Id == "child-trusted-auth" &&
                  resource.Status == "Draft" &&
                  !resource.has_spec
                };

                forbid(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Child
                ) when {
                  resource has ctx_owner_status
                };
                "#,
        )
        .expect("policy should load");

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-trusted-auth",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": "child-trusted-auth",
                    "action": "Create",
                    "params": {
                        "Name": "trusted attributes win",
                        "id": "attacker-selected-id",
                        "status": "Active",
                        "Id": "attacker-selected-Id",
                        "Status": "Deleted",
                        "ctx_owner_status": "Privileged",
                        "has_spec": true
                    }
                }]
            }),
            &agent,
        )
        .await
        .expect("request fields must not spoof trusted Cedar attributes");

    assert!(applied);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_create_collision_authorizes_against_live_durable_fields() {
    let store = SimEventStore::no_faults(47);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let existing = state
        .dispatch_tenant_action(
            &tenant,
            "App",
            "app-live-auth",
            "Create",
            json!({
                "OwnerId": "durable-owner",
                "Name": "original",
                "id": "persisted-spoof-id",
                "status": "Draft",
                "Id": "persisted-spoof-Id",
                "Status": "Draft",
                "ctx_owner_status": "Privileged"
            }),
            &agent,
        )
        .await
        .expect("existing app should be durable");
    assert!(existing.success);
    assert_eq!(
        existing.state.fields.get("Id"),
        Some(&json!("app-live-auth"))
    );
    assert_eq!(existing.state.fields.get("Status"), Some(&json!("Active")));
    assert_eq!(
        existing.state.fields.get("id"),
        Some(&json!("app-live-auth"))
    );
    assert_eq!(existing.state.fields.get("status"), Some(&json!("Active")));
    for reserved in ["has_spec", "ctx_owner_status"] {
        assert!(
            existing.state.fields.get(reserved).is_none(),
            "reserved field {reserved} must not enter live state"
        );
    }

    state
        .authz
        .reload_policies(
            r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is App
                ) when {
                  resource.Id == "app-live-auth" &&
                  resource.Status == "Active" &&
                  resource.OwnerId == "durable-owner"
                };
                "#,
        )
        .expect("policy should load");

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-live-auth",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "App",
                    "entity_id": "app-live-auth",
                    "action": "Create",
                    "params": {
                        "OwnerId": "request-owner",
                        "Name": "updated",
                        "Id": "second-spoof",
                        "Status": "Deleted",
                        "ctx_owner_status": "Privileged"
                    }
                }]
            }),
            &agent,
        )
        .await
        .expect("a colliding Create must authorize against the live durable resource");

    assert!(applied);

    let live = state
        .get_tenant_entity_state(&tenant, "App", "app-live-auth")
        .await
        .expect("updated app should remain readable");
    assert_eq!(live.state.fields.get("Id"), Some(&json!("app-live-auth")));
    assert_eq!(live.state.fields.get("Status"), Some(&json!("Active")));
    assert_eq!(live.state.fields.get("id"), Some(&json!("app-live-auth")));
    assert_eq!(live.state.fields.get("status"), Some(&json!("Active")));
    for reserved in ["has_spec", "ctx_owner_status"] {
        assert!(live.state.fields.get(reserved).is_none());
    }

    let journal = store.dump_journal("default:App:app-live-auth");
    for envelope in journal {
        let event: crate::entity_actor::EntityEvent =
            serde_json::from_value(envelope.payload).expect("app journal event should decode");
        for reserved in [
            "Id",
            "id",
            "Status",
            "status",
            "has_spec",
            "ctx_owner_status",
        ] {
            assert!(
                event.params.get(reserved).is_none(),
                "reserved field {reserved} must not enter the durable event"
            );
        }
    }

    let restarted = composite_test_state_with_store(store);
    let replayed = restarted
        .get_tenant_entity_state(&tenant, "App", "app-live-auth")
        .await
        .expect("app should replay after restart");
    assert_eq!(
        replayed.state.fields.get("Id"),
        Some(&json!("app-live-auth"))
    );
    assert_eq!(replayed.state.fields.get("Status"), Some(&json!("Active")));
    assert_eq!(
        replayed.state.fields.get("id"),
        Some(&json!("app-live-auth"))
    );
    assert_eq!(replayed.state.fields.get("status"), Some(&json!("Active")));
    for reserved in ["has_spec", "ctx_owner_status"] {
        assert!(replayed.state.fields.get(reserved).is_none());
    }
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn replay_canonicalizes_reserved_fields_from_legacy_event_params() {
    let store = SimEventStore::no_faults(48);
    let persistence_id = "default:App:legacy-spoof";
    let event = crate::entity_actor::EntityEvent {
        action: "Create".to_string(),
        from_status: "Active".to_string(),
        to_status: "Active".to_string(),
        timestamp: temper_runtime::scheduler::sim_now(),
        params: json!({
            "OwnerId": "durable-owner",
            "Name": "legacy",
            "Id": "forged-Id",
            "id": "forged-id",
            "Status": "Deleted",
            "status": "Deleted",
            "has_spec": true,
            "ctx_owner_status": "Privileged"
        }),
        idempotency_key: None,
    };
    store
        .append(
            persistence_id,
            0,
            &[temper_runtime::persistence::PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Create".to_string(),
                payload: serde_json::to_value(event).unwrap(),
                metadata: temper_runtime::persistence::EventMetadata {
                    event_id: temper_runtime::scheduler::sim_uuid(),
                    causation_id: temper_runtime::scheduler::sim_uuid(),
                    correlation_id: temper_runtime::scheduler::sim_uuid(),
                    timestamp: temper_runtime::scheduler::sim_now(),
                    actor_id: persistence_id.to_string(),
                },
            }],
        )
        .await
        .unwrap();

    let state = composite_test_state_with_store(store);
    let replayed = state
        .get_tenant_entity_state(&TenantId::default(), "App", "legacy-spoof")
        .await
        .expect("legacy event should replay with canonical fields");
    assert_eq!(
        replayed.state.fields.get("Id"),
        Some(&json!("legacy-spoof"))
    );
    assert_eq!(replayed.state.fields.get("Status"), Some(&json!("Active")));
    assert_eq!(
        replayed.state.fields.get("id"),
        Some(&json!("legacy-spoof"))
    );
    assert_eq!(replayed.state.fields.get("status"), Some(&json!("Active")));
    for reserved in ["has_spec", "ctx_owner_status"] {
        assert!(replayed.state.fields.get(reserved).is_none());
    }
}

#[tokio::test]
async fn composite_ref_sub_write_uses_parent_gate_for_declared_ref_updates() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    state
        .authz
        .reload_tenant_policies_named(
            tenant.as_str(),
            &[(
                "unrelated-child-create".to_string(),
                r#"
                    permit(
                      principal is Agent,
                      action == Action::"Create",
                      resource is Child
                    );
                    "#
                .to_string(),
            )],
        )
        .expect("unrelated tenant policy should load");

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-auth",
            "IngestPack",
            &json!({
                "sub_writes": [{
                    "entity_type": "Ref",
                    "entity_id": "rf-auth-main",
                    "action": "Create",
                    "params": {
                        "RepositoryId": "repo-auth",
                        "Name": "refs/heads/main",
                        "TargetCommitSha": "1111111111111111111111111111111111111111",
                        "Kind": "branch",
                        "PreviousCommitSha": "0000000000000000000000000000000000000000"
                    }
                }]
            }),
            &agent,
        )
        .await
        .expect("declared ref_updates sub-write should use the parent composite gate");

    assert!(applied);
    let reference = state
        .get_tenant_entity_state(&tenant, "Ref", "rf-auth-main")
        .await
        .expect("ref state should be readable");
    assert_eq!(reference.state.status, "Active");
    assert_eq!(
        reference.state.fields.get("TargetCommitSha"),
        Some(&json!("1111111111111111111111111111111111111111"))
    );
}

#[tokio::test]
async fn composite_app_create_sub_write_authorization_can_enforce_owner_scope() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext {
        security_ctx: Some(SecurityContext::from_headers(&[
            ("X-Temper-Principal-Id".to_string(), "alice".to_string()),
            (
                "X-Temper-Principal-Kind".to_string(),
                "customer".to_string(),
            ),
        ])),
        ..Default::default()
    };

    state
        .authz
        .reload_policies(
            r#"
                permit(
                  principal,
                  action == Action::"Create",
                  resource is App
                );

                forbid(
                  principal,
                  action == Action::"Create",
                  resource is App
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Parent.CreateChild" &&
                  !(resource.OwnerId == principal.accountId ||
                    (principal has scopes &&
                     principal.scopes.contains("admin:repos")))
                };
                "#,
        )
        .expect("policy should load");

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-owner-scope",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "App",
                    "entity_id": "app-bob-owned",
                    "action": "Create",
                    "params": { "OwnerId": "bob", "Name": "bob-app" }
                }]
            }),
            &agent,
        )
        .await
        .expect_err("caller must not create a composite App row under another owner")
        .to_string();
    assert!(
        err.contains("sub-write 0 denied"),
        "unexpected error: {err}"
    );
    assert!(!state.entity_exists(&tenant, "App", "app-bob-owned"));

    let allowed = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-owner-scope",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "App",
                    "entity_id": "app-alice-owned",
                    "action": "Create",
                    "params": { "OwnerId": "alice", "Name": "alice-app" }
                }]
            }),
            &agent,
        )
        .await
        .expect("caller should create a composite App row under their own owner");
    assert!(allowed);
    assert!(state.entity_exists(&tenant, "App", "app-alice-owned"));
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_preflights_sub_write_auth_before_persisting_any_write() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    state
        .authz
        .reload_policies(
            r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Child
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Parent.CreateChild" &&
                  resource.id == "child-preflight-first"
                };
                "#,
        )
        .expect("policy should load");

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-preflight",
            "CreateChild",
            &json!({
                "sub_writes": [
                    {
                        "entity_type": "Child",
                        "entity_id": "child-preflight-first",
                        "action": "Create",
                        "params": { "Name": "would be allowed" }
                    },
                    {
                        "entity_type": "Child",
                        "entity_id": "child-preflight-denied",
                        "action": "Create",
                        "params": { "Name": "should be denied" }
                    }
                ]
            }),
            &agent,
        )
        .await
        .expect_err("second sub-write should be denied during preflight")
        .to_string();

    assert!(
        err.contains("sub-write 1 denied"),
        "unexpected error: {err}"
    );
    assert!(
        store
            .dump_journal("default:Child:child-preflight-first")
            .is_empty(),
        "authorized earlier sub-write should not be persisted before later preflight denial"
    );
    assert!(
        store
            .dump_journal("default:Child:child-preflight-denied")
            .is_empty(),
        "denied sub-write should not be persisted"
    );
    assert!(!state.entity_exists(&tenant, "Child", "child-preflight-first"));
    assert!(!state.entity_exists(&tenant, "Child", "child-preflight-denied"));
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_preflights_sub_write_transition_before_persisting_any_write() {
    let store = SimEventStore::no_faults(41);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let existing = state
        .dispatch_tenant_action(
            &tenant,
            "Child",
            "child-transition-existing",
            "Create",
            json!({ "Name": "already active" }),
            &agent,
        )
        .await
        .expect("existing child create should run");
    assert!(existing.success);

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-transition-preflight",
            "CreateChild",
            &json!({
                "sub_writes": [
                    {
                        "entity_type": "Child",
                        "entity_id": "child-transition-first",
                        "action": "Create",
                        "params": { "Name": "would otherwise persist first" }
                    },
                    {
                        "entity_type": "Child",
                        "entity_id": "child-transition-existing",
                        "action": "Create",
                        "params": { "Name": "invalid from Active" }
                    }
                ]
            }),
            &agent,
        )
        .await
        .expect_err("second sub-write should fail transition preflight")
        .to_string();

    assert!(
        err.contains("sub-write 1 would fail"),
        "unexpected error: {err}"
    );
    assert!(
        store
            .dump_journal("default:Child:child-transition-first")
            .is_empty(),
        "earlier sub-write should not persist before later transition preflight failure"
    );
    assert!(
        !state.entity_exists(&tenant, "Child", "child-transition-first"),
        "earlier sub-write actor should not be spawned"
    );
    assert_eq!(
        store
            .dump_journal("default:Child:child-transition-existing")
            .len(),
        2,
        "existing target should keep only its bootstrap and original Create events"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_atomic_batch_conflict_leaves_all_sub_write_journals_empty() {
    let store = SimEventStore::no_faults(42);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    store.inject_concurrency_violations("default:Child:child-atomic-second", 1);

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-atomic-batch",
            "CreateChild",
            &json!({
                "sub_writes": [
                    {
                        "entity_type": "Child",
                        "entity_id": "child-atomic-first",
                        "action": "Create",
                        "params": { "Name": "must not persist" }
                    },
                    {
                        "entity_type": "Child",
                        "entity_id": "child-atomic-second",
                        "action": "Create",
                        "params": { "Name": "injected conflict" }
                    }
                ]
            }),
            &agent,
        )
        .await
        .expect_err("atomic batch conflict should reject the whole composite")
        .to_string();

    assert!(
        err.contains("composite batch persistence conflict"),
        "unexpected error: {err}"
    );
    assert!(
        store
            .dump_journal("default:Child:child-atomic-first")
            .is_empty(),
        "first sub-write journal must stay empty when a later stream conflicts"
    );
    assert!(
        store
            .dump_journal("default:Child:child-atomic-second")
            .is_empty(),
        "conflicting sub-write journal must also stay empty"
    );
    assert!(!state.entity_exists(&tenant, "Child", "child-atomic-first"));
    assert!(!state.entity_exists(&tenant, "Child", "child-atomic-second"));
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_atomic_batch_records_parent_composite_event_once() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let callback_params = json!({
        "sub_writes": [{
            "entity_type": "Child",
            "entity_id": "child-composite-event",
            "action": "Create",
            "params": { "Name": "recorded through CompositeEvent" }
        }]
    });

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-composite-event",
            "CreateChild",
            &callback_params,
            &agent,
        )
        .await
        .expect("composite result should apply");

    let parent_pid = "default:Parent:parent-composite-event";
    let parent_journal = store.dump_journal(parent_pid);
    assert_eq!(
        parent_journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", COMPOSITE_EVENT_TYPE]
    );
    let composite_event =
        serde_json::from_value::<CompositeEvent>(parent_journal[1].payload.clone())
            .expect("CompositeEvent payload should decode");
    assert_eq!(composite_event.parent_entity_type, "Parent");
    assert_eq!(composite_event.parent_entity_id, "parent-composite-event");
    assert_eq!(composite_event.parent_action, "CreateChild");
    assert_eq!(composite_event.sub_writes.len(), 1);
    assert_eq!(composite_event.sub_writes[0].entity_type, "Child");
    assert_eq!(
        composite_event.sub_writes[0].entity_id,
        "child-composite-event"
    );
    assert_eq!(composite_event.sub_writes[0].action, "Create");
    assert!(
        composite_event.sub_writes[0]
            .idempotency_key
            .contains("subwrite:0")
    );

    let restarted = composite_test_state_with_store(store.clone());
    let parent = restarted
        .get_tenant_entity_state(&tenant, "Parent", "parent-composite-event")
        .await
        .expect("parent should hydrate from journal");
    assert_eq!(parent.state.status, "Active");
    assert_eq!(parent.state.sequence_nr, 2);
    assert!(parent.state.fields.get("sub_writes").is_none());

    restarted
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-composite-event",
            "CreateChild",
            &callback_params,
            &agent,
        )
        .await
        .expect("duplicate composite result should be idempotent");
    assert_eq!(
        store.dump_journal(parent_pid).len(),
        parent_journal.len(),
        "duplicate composite callback must not append a second CompositeEvent"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_idempotency_lookup_is_bounded_on_long_parent_journal() {
    use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
    use temper_runtime::scheduler::{sim_now, sim_uuid};

    let store = SimEventStore::no_faults(40_001);
    let persistence_id = "default:Parent:long-idempotency-journal";
    let mut events = (0..1_500)
        .map(|_| PersistenceEnvelope {
            sequence_nr: 0,
            event_type: "AuditNoise".to_string(),
            payload: json!({}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: persistence_id.to_string(),
            },
        })
        .collect::<Vec<_>>();
    events.push(
        composite_event_envelope(
            persistence_id,
            &CompositeEvent {
                tenant: "default".to_string(),
                parent_entity_type: "Parent".to_string(),
                parent_entity_id: "long-idempotency-journal".to_string(),
                parent_action: "CreateChild".to_string(),
                composite_idempotency_key: "recent-composite-key".to_string(),
                sub_writes: Vec::new(),
            },
        )
        .unwrap(),
    );
    store.append(persistence_id, 0, &events).await.unwrap();

    let state = composite_test_state_with_store(store.clone());
    let boxed = crate::storage::BoxedEventStore::new(store);
    assert!(
        state
            .composite_event_already_persisted(&boxed, persistence_id, "recent-composite-key",)
            .await
            .unwrap(),
        "the bounded recent window must find a duplicate without scanning lifetime history"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_atomic_batch_can_skip_parent_composite_event_by_spec() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let callback_params = json!({
        "sub_writes": [{
            "entity_type": "Child",
            "entity_id": "child-no-parent-event",
            "action": "Create",
            "params": { "Name": "recorded only on child" }
        }]
    });

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-no-composite-event",
            "CreateChildWithoutParentEvent",
            &callback_params,
            &agent,
        )
        .await
        .expect("composite result should apply without parent event");

    assert!(
        store
            .dump_journal("default:Parent:parent-no-composite-event")
            .is_empty(),
        "record_parent_event=false should leave the parent journal untouched"
    );
    assert_eq!(
        store
            .dump_journal("default:Child:child-no-parent-event")
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Create"]
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn parent_gated_pack_object_create_repairs_partial_existing_object() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let mut agent = AgentContext::for_service("composite-test");
    agent.idempotency_key = Some("legacy-partial-pack".to_string());
    let blob_id = "rp-test-abc123";
    let blob_pid = format!("default:Blob:{blob_id}");

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &json!({
                "sub_writes": [{
                    "entity_type": "Blob",
                    "entity_id": blob_id,
                    "action": "Create",
                    "params": {
                        "Id": "abc123",
                        "RepositoryId": "rp-test"
                    }
                }]
            }),
            &agent,
        )
        .await
        .expect("partial legacy pack object should stage");

    assert_eq!(store.dump_journal(&blob_pid).len(), 1);

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &json!({
                "sub_writes": [{
                    "entity_type": "Blob",
                    "entity_id": blob_id,
                    "action": "Create",
                    "params": {
                        "Id": "abc123",
                        "RepositoryId": "rp-test",
                        "CanonicalBytes": "YmxvYiAwAA=="
                    }
                }]
            }),
            &agent,
        )
        .await
        .expect("complete pack object should repair the partial stream");

    let blob = state
        .get_tenant_entity_state(&tenant, "Blob", blob_id)
        .await
        .expect("repaired blob should be readable");
    assert_eq!(
        blob.state.fields.get("CanonicalBytes"),
        Some(&json!("YmxvYiAwAA=="))
    );
    assert_eq!(
        store.dump_journal(&blob_pid).len(),
        2,
        "repair appends at the current sequence instead of expecting zero"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn parent_gated_pack_object_create_skips_complete_existing_object() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let mut agent = AgentContext::for_service("composite-test");
    let blob_id = "rp-test-def456";
    let blob_pid = format!("default:Blob:{blob_id}");
    let callback_params = json!({
        "sub_writes": [{
            "entity_type": "Blob",
            "entity_id": blob_id,
            "action": "Create",
            "params": {
                "Id": "def456",
                "RepositoryId": "rp-test",
                "CanonicalBytes": "YmxvYiAwAA=="
            }
        }]
    });

    agent.idempotency_key = Some("first-pack".to_string());
    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &callback_params,
            &agent,
        )
        .await
        .expect("first complete object write should append");
    let first_len = store.dump_journal(&blob_pid).len();

    agent.idempotency_key = Some("second-pack".to_string());
    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &callback_params,
            &agent,
        )
        .await
        .expect("complete duplicate object should no-op");

    assert_eq!(
        store.dump_journal(&blob_pid).len(),
        first_len,
        "complete pack objects should not accumulate duplicate Create events"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_ref_create_cas_rejects_existing_ref_without_pack_object_leak() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let ref_id = "ref-main-create-cas";
    let old_sha = "1111111111111111111111111111111111111111";
    let new_sha = "2222222222222222222222222222222222222222";

    let created = state
        .dispatch_tenant_action(
            &tenant,
            "Ref",
            ref_id,
            "Create",
            json!({
                "RepositoryId": "repo-test",
                "Name": "refs/heads/main",
                "TargetCommitSha": old_sha,
                "Kind": "branch"
            }),
            &agent,
        )
        .await
        .expect("existing ref create should run");
    assert!(created.success);

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &json!({
                "sub_writes": [
                    {
                        "entity_type": "Blob",
                        "entity_id": "repo-test-cas-create-blob",
                        "action": "Create",
                        "params": {
                            "Id": "cas-create-blob",
                            "RepositoryId": "repo-test",
                            "CanonicalBytes": "YmxvYiAwAA=="
                        }
                    },
                    {
                        "entity_type": "Ref",
                        "entity_id": ref_id,
                        "action": "Create",
                        "params": {
                            "RepositoryId": "repo-test",
                            "Name": "refs/heads/main",
                            "PreviousCommitSha": "0000000000000000000000000000000000000000",
                            "TargetCommitSha": new_sha,
                            "Kind": "branch"
                        }
                    }
                ]
            }),
            &agent,
        )
        .await
        .expect_err("stale ref create should fail before appending pack objects")
        .to_string();

    assert!(err.contains("stale ref"), "unexpected error: {err}");
    assert!(
        store
            .dump_journal("default:Blob:repo-test-cas-create-blob")
            .is_empty(),
        "losing pack object must not persist when the ref create CAS fails"
    );
    let ref_state = state
        .get_tenant_entity_state(&tenant, "Ref", ref_id)
        .await
        .expect("original ref should remain readable");
    assert_eq!(
        ref_state.state.fields.get("TargetCommitSha"),
        Some(&json!(old_sha))
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_ref_update_cas_rejects_stale_previous_without_pack_object_leak() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let ref_id = "ref-main-update-cas";
    let current_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let stale_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let new_sha = "cccccccccccccccccccccccccccccccccccccccc";

    let created = state
        .dispatch_tenant_action(
            &tenant,
            "Ref",
            ref_id,
            "Create",
            json!({
                "RepositoryId": "repo-test",
                "Name": "refs/heads/main",
                "TargetCommitSha": current_sha,
                "Kind": "branch"
            }),
            &agent,
        )
        .await
        .expect("existing ref create should run");
    assert!(created.success);

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &json!({
                "sub_writes": [
                    {
                        "entity_type": "Blob",
                        "entity_id": "repo-test-cas-update-blob",
                        "action": "Create",
                        "params": {
                            "Id": "cas-update-blob",
                            "RepositoryId": "repo-test",
                            "CanonicalBytes": "YmxvYiAxAA=="
                        }
                    },
                    {
                        "entity_type": "Ref",
                        "entity_id": ref_id,
                        "action": "Update",
                        "params": {
                            "PreviousCommitSha": stale_sha,
                            "NewCommitSha": new_sha,
                            "TargetCommitSha": new_sha
                        }
                    }
                ]
            }),
            &agent,
        )
        .await
        .expect_err("stale ref update should fail before appending pack objects")
        .to_string();

    assert!(err.contains("stale ref"), "unexpected error: {err}");
    assert!(
        store
            .dump_journal("default:Blob:repo-test-cas-update-blob")
            .is_empty(),
        "losing pack object must not persist when the ref update CAS fails"
    );
    let ref_state = state
        .get_tenant_entity_state(&tenant, "Ref", ref_id)
        .await
        .expect("original ref should remain readable");
    assert_eq!(
        ref_state.state.fields.get("TargetCommitSha"),
        Some(&json!(current_sha))
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_sub_write_idempotency_survives_actor_restart() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let callback_params = json!({
        "sub_writes": [{
            "entity_type": "Child",
            "entity_id": "child-replay",
            "action": "Create",
            "params": { "Name": "created once" }
        }]
    });

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            &callback_params,
            &agent,
        )
        .await
        .expect("first composite result should apply");
    assert!(applied);

    let child_pid = "default:Child:child-replay";
    let first_journal_len = store.dump_journal(child_pid).len();
    assert!(
        first_journal_len >= 2,
        "child journal should contain bootstrap + Create event"
    );

    let restarted = composite_test_state_with_store(store.clone());
    let replayed = restarted
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            &callback_params,
            &agent,
        )
        .await
        .expect("duplicate composite result should be idempotent after replay");
    assert!(replayed);

    let child = restarted
        .get_tenant_entity_state(&tenant, "Child", "child-replay")
        .await
        .expect("child should still be readable");
    assert_eq!(child.state.status, "Active");
    assert_eq!(child.state.fields.get("Name"), Some(&json!("created once")));
    assert_eq!(
        store.dump_journal(child_pid).len(),
        first_journal_len,
        "duplicate sub-write should not append a second Create event"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_batches_replace_declared_keys_on_create_update_and_delete() {
    let store = SimEventStore::no_faults(39);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-key-test");
    let child_id = "child-composite-key-lifecycle";
    let key_hash = |name: &str| {
        crate::key_index::canonical_key_hash(
            "name",
            &["Name".to_string()],
            serde_json::json!({"Name": name}).as_object().unwrap(),
        )
        .unwrap()
    };

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-key-lifecycle",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": child_id,
                    "action": "Create",
                    "params": {"Name": "before"}
                }]
            }),
            &agent,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .lookup_by_key("default", "Child", "name", &key_hash("before"))
            .await
            .unwrap(),
        Some(child_id.to_string())
    );

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-key-lifecycle",
            "RenameChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": child_id,
                    "action": "Rename",
                    "params": {"Name": "after"}
                }]
            }),
            &agent,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .lookup_by_key("default", "Child", "name", &key_hash("before"))
            .await
            .unwrap(),
        None,
        "batch update must retire the old key claim"
    );
    assert_eq!(
        store
            .lookup_by_key("default", "Child", "name", &key_hash("after"))
            .await
            .unwrap(),
        Some(child_id.to_string())
    );

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-key-lifecycle",
            "DeleteChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": child_id,
                    "action": "Delete",
                    "params": {}
                }]
            }),
            &agent,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .lookup_by_key("default", "Child", "name", &key_hash("after"))
            .await
            .unwrap(),
        None
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_atomic_batch_allows_existing_sub_write_to_delete_target() {
    let store = SimEventStore::no_faults(40);
    let projection_path = std::env::temp_dir().join(format!(
        "temper-composite-delete-projection-{}.db",
        temper_runtime::scheduler::sim_uuid()
    ));
    let _ = std::fs::remove_file(&projection_path);
    let projection_store =
        TursoEventStore::new(&format!("file:{}", projection_path.display()), None)
            .await
            .expect("create projection store");
    let mut state = composite_test_state();
    let mut storage = StorageStack::from_sim(store.clone(), None);
    storage.query_plane = Some(Arc::new(projection_store.clone()));
    state.set_storage_stack(storage);
    *state.query_projection_queue.lock().unwrap() = None;
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let child_id = "child-delete-through-composite";

    let created = state
        .dispatch_tenant_action(
            &tenant,
            "Child",
            child_id,
            "Create",
            json!({ "Name": "temporary child" }),
            &agent,
        )
        .await
        .expect("child create should run");
    assert!(created.success);
    assert!(state.entity_exists(&tenant, "Child", child_id));
    let child_key_hash = crate::key_index::canonical_key_hash(
        "name",
        &["Name".to_string()],
        created.state.fields.as_object().unwrap(),
    )
    .expect("created child has declared key");
    assert_eq!(
        store
            .lookup_by_key("default", "Child", "name", &child_key_hash)
            .await
            .unwrap(),
        Some(child_id.to_string())
    );

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-delete-child",
            "DeleteChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": child_id,
                    "action": "Delete",
                    "params": {}
                }]
            }),
            &agent,
        )
        .await
        .expect("composite delete sub-write should commit without reloading a tombstone");
    assert!(applied);

    assert!(
        !state.ensure_entity_loaded(&tenant, "Child", child_id).await,
        "deleted composite sub-write target should not be reloaded as a live entity"
    );
    assert!(!state.entity_exists(&tenant, "Child", child_id));
    assert!(
        state
            .list_entity_ids_lazy(&tenant, "Child")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .lookup_by_key("default", "Child", "name", &child_key_hash)
            .await
            .unwrap(),
        None,
        "composite tombstone must retire the declared key claim"
    );
    assert!(
        projection_store
            .query_field_index(
                "default",
                "Child",
                "field_name = ?3 AND field_value = ?4",
                vec!["Name".to_string(), "temporary child".to_string()],
            )
            .await
            .unwrap()
            .is_empty(),
        "composite delete must remove the durable query projection"
    );

    let child_journal = store.dump_journal(&format!("default:Child:{child_id}"));
    assert_eq!(
        child_journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Create", "Delete"]
    );
    assert!(temper_runtime::persistence::is_deletion_tombstone(
        child_journal.last().unwrap()
    ));
    let _ = std::fs::remove_file(projection_path);
}

#[cfg(feature = "sim")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn composite_ingest_pack_large_blob_sub_write_persists_overflow_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SimEventStore::no_faults(44);
    let mut state = composite_test_state_with_store(store.clone());
    state.data_dir = dir.path().to_path_buf();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let canonical_bytes = "W".repeat(512 * 1024);

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-large-blob",
            "IngestPack",
            &json!({
                "sub_writes": [{
                    "entity_type": "Blob",
                    "entity_id": "blob-large-1",
                    "action": "Create",
                    "params": {
                        "RepositoryId": "repo-large-blob",
                        "CanonicalBytes": canonical_bytes
                    }
                }]
            }),
            &agent,
        )
        .await
        .expect("large Blob sub-write should persist through field-overflow");
    assert!(applied);

    let blob = state
        .get_tenant_entity_state(&tenant, "Blob", "blob-large-1")
        .await
        .expect("large blob entity should be readable");
    let canonical_field = blob
        .state
        .fields
        .get("CanonicalBytes")
        .expect("CanonicalBytes field should be present");
    let blob_key = canonical_field
        .get(crate::blobs::FIELD_OVERFLOW_REF_KEY)
        .and_then(serde_json::Value::as_str)
        .expect("large CanonicalBytes should be stored as a field-overflow blob ref");
    let bytes = state
        .get_blob_with_legacy_fallback(&tenant, blob_key)
        .await
        .expect("field-overflow blob read should succeed")
        .expect("field-overflow blob should exist");
    let restored: serde_json::Value =
        serde_json::from_slice(&bytes).expect("field-overflow blob should contain JSON");
    assert_eq!(
        restored.as_str().map(str::len),
        Some(512 * 1024),
        "field-overflow blob should preserve the full large field"
    );

    let blob_journal = store.dump_journal("default:Blob:blob-large-1");
    assert!(
        blob_journal
            .iter()
            .any(|event| event.event_type == "Create"),
        "atomic composite batch should persist the Blob.Create event"
    );
}

#[cfg(feature = "sim")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn composite_atomic_batch_handles_concurrent_multi_entity_results() {
    const COMPOSITES: usize = 12;
    const CHILDREN_PER_COMPOSITE: usize = 3;

    let store = SimEventStore::no_faults(44);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let mut handles = Vec::new();
    for composite_idx in 0..COMPOSITES {
        let state = state.clone();
        let tenant = tenant.clone();
        let agent = agent.clone();
        handles.push(tokio::spawn(async move {
            let parent_id = format!("parent-stress-{composite_idx}");
            let mut sub_writes = Vec::new();
            for child_idx in 0..CHILDREN_PER_COMPOSITE {
                sub_writes.push(json!({
                    "entity_type": "Child",
                    "entity_id": format!("child-stress-{composite_idx}-{child_idx}"),
                    "action": "Create",
                    "params": {
                        "Name": format!("child {composite_idx}/{child_idx}")
                    }
                }));
            }
            sub_writes.push(json!({
                "entity_type": "App",
                "entity_id": format!("app-stress-{composite_idx}"),
                "action": "Create",
                "params": {
                    "OwnerId": format!("owner-{composite_idx}"),
                    "Name": format!("app-{composite_idx}")
                }
            }));

            let applied = state
                .apply_composite_integration_result(
                    &tenant,
                    "Parent",
                    &parent_id,
                    "CreateChild",
                    &json!({ "sub_writes": sub_writes }),
                    &agent,
                )
                .await
                .map_err(|err| err.to_string())?;
            Ok::<_, String>((parent_id, applied))
        }));
    }

    let mut parent_ids = Vec::new();
    for handle in handles {
        let (parent_id, applied) = handle
            .await
            .expect("concurrent composite task should join")
            .expect("concurrent composite result should apply");
        assert!(applied);
        parent_ids.push(parent_id);
    }

    for parent_id in parent_ids {
        let composite_idx = parent_id
            .strip_prefix("parent-stress-")
            .expect("stress parent id should include numeric suffix")
            .parse::<usize>()
            .expect("stress parent suffix should parse");
        let parent_journal = store.dump_journal(&format!("default:Parent:{parent_id}"));
        assert_eq!(
            parent_journal
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec!["Created", COMPOSITE_EVENT_TYPE],
            "parent {parent_id} should record one replay-safe CompositeEvent"
        );
        let composite_event =
            serde_json::from_value::<CompositeEvent>(parent_journal[1].payload.clone())
                .expect("CompositeEvent payload should decode");
        assert_eq!(composite_event.sub_writes.len(), CHILDREN_PER_COMPOSITE + 1);

        for child_idx in 0..CHILDREN_PER_COMPOSITE {
            let child_id = format!("child-stress-{composite_idx}-{child_idx}");
            let child = state
                .get_tenant_entity_state(&tenant, "Child", &child_id)
                .await
                .expect("stress child should be readable");
            assert_eq!(child.state.status, "Active");
            assert_eq!(
                child.state.fields.get("Name"),
                Some(&json!(format!("child {composite_idx}/{child_idx}")))
            );
        }

        let app_id = format!("app-stress-{composite_idx}");
        let app = state
            .get_tenant_entity_state(&tenant, "App", &app_id)
            .await
            .expect("stress app should be readable");
        assert_eq!(
            app.state.fields.get("OwnerId"),
            Some(&json!(format!("owner-{composite_idx}")))
        );
        assert_eq!(
            app.state.fields.get("Name"),
            Some(&json!(format!("app-{composite_idx}")))
        );
    }
}

#[tokio::test]
async fn commons_composite_rejects_duplicate_owner_app_name_before_dispatch() {
    let state = composite_test_state();
    state.enable_commons_guardrails("default");
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let first = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-app-name",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "App",
                    "entity_id": "app-alice-notes",
                    "action": "Create",
                    "params": { "OwnerId": "alice", "Name": "notes" }
                }]
            }),
            &agent,
        )
        .await
        .expect("first owner/app name should apply");
    assert!(first);

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-app-name",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "App",
                    "entity_id": "app-alice-notes-copy",
                    "action": "Create",
                    "params": { "OwnerId": "Alice", "Name": "Notes" }
                }]
            }),
            &agent,
        )
        .await
        .expect_err("duplicate owner/app name should be rejected")
        .to_string();

    assert!(
        err.contains("alice/Notes") || err.contains("Alice/Notes"),
        "unexpected error: {err}"
    );
    assert!(!state.entity_exists(&tenant, "App", "app-alice-notes-copy"));
}

#[cfg(feature = "sim")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commons_composite_app_name_uniqueness_serializes_concurrent_creates() {
    let store = SimEventStore::no_faults(43);
    let state = composite_test_state_with_store(store.clone());
    state.enable_commons_guardrails("default");
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let attempts = [
        ("parent-app-race-a", "app-race-a"),
        ("parent-app-race-b", "app-race-b"),
    ];

    let mut handles = Vec::new();
    for (parent_id, app_id) in attempts {
        let state = state.clone();
        let tenant = tenant.clone();
        let agent = agent.clone();
        handles.push(tokio::spawn(async move {
            let result = state
                .apply_composite_integration_result(
                    &tenant,
                    "Parent",
                    parent_id,
                    "CreateChild",
                    &json!({
                        "sub_writes": [{
                            "entity_type": "App",
                            "entity_id": app_id,
                            "action": "Create",
                            "params": { "OwnerId": "alice", "Name": "Notes" }
                        }]
                    }),
                    &agent,
                )
                .await
                .map_err(|err| err.to_string());
            (parent_id.to_string(), app_id.to_string(), result)
        }));
    }

    let mut outcomes = Vec::new();
    for handle in handles {
        outcomes.push(handle.await.expect("concurrent task should finish"));
    }

    let successes = outcomes
        .iter()
        .filter(|(_, _, result)| matches!(result, Ok(true)))
        .count();
    let conflicts = outcomes
        .iter()
        .filter(|(_, _, result)| matches!(result, Err(err) if err.contains("already registered")))
        .count();
    assert_eq!(
        successes, 1,
        "exactly one concurrent composite should create alice/Notes: {outcomes:?}"
    );
    assert_eq!(
        conflicts, 1,
        "the racing composite should fail closed with an app-name conflict: {outcomes:?}"
    );

    let persisted_apps = outcomes
        .iter()
        .filter(|(_, app_id, _)| state.entity_exists(&tenant, "App", app_id))
        .collect::<Vec<_>>();
    assert_eq!(
        persisted_apps.len(),
        1,
        "only the winning App row should exist after the race"
    );

    for (parent_id, app_id, result) in outcomes {
        let parent_journal = store.dump_journal(&format!("default:Parent:{parent_id}"));
        match result {
            Ok(true) => {
                assert_eq!(
                    parent_journal
                        .iter()
                        .map(|event| event.event_type.as_str())
                        .collect::<Vec<_>>(),
                    vec!["Created", COMPOSITE_EVENT_TYPE],
                    "winning parent should record exactly one CompositeEvent"
                );
                let app = state
                    .get_tenant_entity_state(&tenant, "App", &app_id)
                    .await
                    .expect("winning app should be readable");
                assert_eq!(app.state.fields.get("OwnerId"), Some(&json!("alice")));
                assert_eq!(app.state.fields.get("Name"), Some(&json!("Notes")));
            }
            Err(err) => {
                assert!(
                    err.contains("already registered"),
                    "unexpected losing result: {err}"
                );
                assert!(
                    parent_journal.is_empty(),
                    "losing parent journal must remain empty when uniqueness preflight rejects it"
                );
                assert!(
                    !state.entity_exists(&tenant, "App", &app_id),
                    "losing App row must not be persisted"
                );
            }
            Ok(false) => panic!("composite should not fall back for simple App.Create"),
        }
    }
}

#[tokio::test]
async fn composite_integration_result_rejects_undeclared_sub_write() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Parent",
                    "entity_id": "parent-2",
                    "action": "CreateChild",
                    "params": {}
                }]
            }),
            &agent,
        )
        .await
        .expect_err("undeclared sub-write should be rejected");

    let err = err.to_string();
    assert!(err.contains("is not declared"), "unexpected error: {err}");
}
