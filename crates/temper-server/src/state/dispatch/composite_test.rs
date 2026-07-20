use std::collections::BTreeMap;

use serde_json::json;
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;
#[cfg(feature = "sim")]
use temper_store_sim::SimEventStore;

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

#[path = "composite_test/atomic.rs"]
mod atomic;
#[path = "composite_test/basic.rs"]
mod basic;
#[path = "composite_test/cas.rs"]
mod cas;
#[path = "composite_test/concurrency.rs"]
mod concurrency;
