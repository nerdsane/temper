use std::collections::BTreeMap;

use serde_json::json;
use temper_runtime::ActorSystem;
#[cfg(feature = "sim")]
use temper_runtime::persistence::{EventStore, PersistenceError};
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

[[action]]
name = "CreateChildWithEffect"
kind = "Composite"
from = ["Active"]
to = "Active"
params = ["Reason"]

[[action.sub_writes]]
target_entity = "Child"
action = "CreateWithEffect"
generated_from = "child"
"#;

const CHILD_IOA: &str = r#"
[automaton]
name = "Child"
states = ["Draft", "Active", "Deleted"]
initial = "Draft"

[[state]]
name = "revision"
type = "counter"
initial = "0"

[[key]]
name = "child_name"
properties = ["Name"]

[[action]]
name = "Create"
kind = "input"
from = ["Draft"]
to = "Active"
params = ["Name"]
effect = [{ type = "increment", var = "revision" }]

[[action]]
name = "Delete"
kind = "input"
from = ["Active"]
to = "Deleted"
params = []

[[action]]
name = "CreateWithEffect"
kind = "input"
from = ["Draft"]
to = "Active"
params = ["Name"]
effect = [{ type = "schedule", action = "Delete", delay_seconds = 2700 }]
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

#[cfg(feature = "sim")]
#[derive(Clone)]
struct PauseFirstAtomicStableLoadStore {
    inner: SimEventStore,
    target_boundary_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    reached: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
}

#[cfg(feature = "sim")]
impl PauseFirstAtomicStableLoadStore {
    fn new(inner: SimEventStore) -> Self {
        Self {
            inner,
            target_boundary_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            reached: std::sync::Arc::new(tokio::sync::Notify::new()),
            resume: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    async fn wait_until_first_atomic_load_is_paused(&self) {
        self.reached.notified().await;
    }

    fn resume_first_atomic_load(&self) {
        self.resume.notify_one();
    }
}

#[cfg(feature = "sim")]
impl EventStore for PauseFirstAtomicStableLoadStore {
    fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[temper_runtime::persistence::PersistenceEnvelope],
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        self.inner.append(persistence_id, expected_sequence, events)
    }

    fn append_batch(
        &self,
        appends: &[temper_runtime::persistence::PersistenceAppend],
    ) -> impl std::future::Future<
        Output = Result<
            Vec<temper_runtime::persistence::PersistenceAppendResult>,
            PersistenceError,
        >,
    > + Send {
        self.inner.append_batch(appends)
    }

    fn batch_idempotency_committed(
        &self,
        claim: &temper_runtime::persistence::PersistenceBatchIdempotency,
    ) -> impl std::future::Future<Output = Result<bool, PersistenceError>> + Send {
        self.inner.batch_idempotency_committed(claim)
    }

    fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> impl std::future::Future<
        Output = Result<Vec<temper_runtime::persistence::PersistenceEnvelope>, PersistenceError>,
    > + Send {
        self.inner.read_events(persistence_id, from_sequence)
    }

    fn read_events_page(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        through_sequence: u64,
        limit: usize,
    ) -> impl std::future::Future<
        Output = Result<Vec<temper_runtime::persistence::PersistenceEnvelope>, PersistenceError>,
    > + Send {
        self.inner
            .read_events_page(persistence_id, from_sequence, through_sequence, limit)
    }

    async fn journal_boundary(
        &self,
        persistence_id: &str,
    ) -> Result<temper_runtime::persistence::JournalBoundary, PersistenceError> {
        if persistence_id == "default:Child:concurrent-exact-child" {
            let call = self
                .target_boundary_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 2 {
                self.reached.notify_one();
                self.resume.notified().await;
            }
        }
        self.inner.journal_boundary(persistence_id).await
    }

    fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        self.inner
            .save_snapshot(persistence_id, sequence_nr, snapshot)
    }

    fn save_snapshot_if_source(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
        source: &temper_runtime::persistence::SnapshotSourceFence,
        key_contract: Option<&str>,
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        self.inner.save_snapshot_if_source(
            persistence_id,
            sequence_nr,
            snapshot,
            source,
            key_contract,
        )
    }

    fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<(u64, Vec<u8>)>, PersistenceError>> + Send
    {
        self.inner.load_snapshot(persistence_id)
    }

    fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> impl std::future::Future<Output = Result<Vec<(String, String)>, PersistenceError>> + Send
    {
        self.inner.list_entity_ids(tenant)
    }

    fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
        self.inner.list_entity_ids_by_type(tenant, entity_type)
    }
}

#[cfg(feature = "sim")]
fn composite_test_state_with_paused_atomic_load_store(
    store: PauseFirstAtomicStableLoadStore,
) -> ServerState {
    let mut state = composite_test_state();
    state.set_storage_stack(StorageStack::new(
        crate::storage::BackendLabel::Sim,
        crate::storage::BoxedEventStore::new(store),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ));
    state
}

#[cfg(feature = "sim")]
fn composite_registry_test_state_with_store(store: SimEventStore) -> ServerState {
    let csdl = parse_csdl(COMPOSITE_CSDL).expect("test CSDL should parse");
    let mut registry = crate::registry::SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        COMPOSITE_CSDL.to_string(),
        &[
            ("Parent", PARENT_IOA),
            ("Child", CHILD_IOA),
            ("App", APP_IOA),
            ("Blob", BLOB_IOA),
            ("Ref", REF_IOA),
        ],
    );
    let mut state = ServerState::from_registry(
        ActorSystem::new("composite-registry-dispatch-test"),
        registry,
    );
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
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
async fn composite_exact_retry_is_content_bound_and_repairs_runtime_convergence() {
    let store = SimEventStore::no_faults(401);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let mut agent = AgentContext::for_service("composite-retry-test");
    agent.idempotency_key = Some("stable-parent-operation".to_string());
    let child_id = "child-content-bound-retry";
    let callback = json!({
        "sub_writes": [{
            "entity_type": "Child",
            "entity_id": child_id,
            "action": "Create",
            "params": { "Name": "original value" }
        }]
    });
    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-content-bound-retry",
            "CreateChild",
            &callback,
            &agent,
        )
        .await
        .expect("first composite must commit");
    let child_pid = format!("default:Child:{child_id}");
    let first_journal_len = store.dump_journal(&child_pid).len();

    state.cache_entity_status(child_pid.clone(), "StaleProjection".to_string());
    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-content-bound-retry",
            "CreateChild",
            &callback,
            &agent,
        )
        .await
        .expect("exact retry must finish post-commit convergence");
    assert_eq!(store.dump_journal(&child_pid).len(), first_journal_len);
    assert_eq!(
        state
            .entity_state_cache
            .lock()
            .expect("state cache lock")
            .get(&child_pid)
            .map(|(status, _)| status.as_str()),
        Some("Active"),
        "an exact durable retry must refresh derived runtime state"
    );

    let error = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-content-bound-retry",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": child_id,
                    "action": "Create",
                    "params": { "Name": "different value" }
                }]
            }),
            &agent,
        )
        .await
        .expect_err("one parent idempotency key cannot authorize different sub-write values");
    assert!(error.to_string().contains("different intent"));
    assert_eq!(store.dump_journal(&child_pid).len(), first_journal_len);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_exact_retry_uses_durable_claim_after_actor_history_ages_out() {
    let store = SimEventStore::no_faults(402);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let mut agent = AgentContext::for_service("composite-aged-retry-test");
    agent.idempotency_key = Some("aged-parent-operation".to_string());
    let child_id = "child-aged-content-bound-retry";
    let callback = json!({
        "sub_writes": [{
            "entity_type": "Child",
            "entity_id": child_id,
            "action": "Create",
            "params": { "Name": "durable value" }
        }]
    });

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-aged-content-bound-retry",
            "CreateChild",
            &callback,
            &agent,
        )
        .await
        .expect("first composite must commit");
    let child_pid = format!("default:Child:{child_id}");
    let expected_sequence = store.dump_journal(&child_pid).len() as u64;
    let aged_events = (0..=crate::entity_actor::types::MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY)
        .map(|index| {
            composite_envelope(
                &child_pid,
                &crate::entity_actor::EntityEvent {
                    action: "AgedHistory".to_string(),
                    from_status: "Active".to_string(),
                    to_status: "Active".to_string(),
                    timestamp: temper_runtime::scheduler::sim_now(),
                    params: json!({"index": index}),
                    idempotency_key: Some(format!("aged-history-{index}")),
                },
            )
            .expect("encode aged event")
        })
        .collect::<Vec<_>>();
    store
        .append(&child_pid, expected_sequence, &aged_events)
        .await
        .expect("advance child beyond bounded actor idempotency history");
    state.stop_and_remove_entity(&tenant, "Child", child_id);
    state.cache_entity_status(child_pid.clone(), "StaleProjection".to_string());
    let journal_len = store.dump_journal(&child_pid).len();

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-aged-content-bound-retry",
            "CreateChild",
            &callback,
            &agent,
        )
        .await
        .expect("durable claim must bypass current-state guards after actor history eviction");

    assert_eq!(store.dump_journal(&child_pid).len(), journal_len);
    assert_eq!(
        state
            .entity_state_cache
            .lock()
            .expect("state cache lock")
            .get(&child_pid)
            .map(|(status, _)| status.as_str()),
        Some("Active"),
        "exact replay must still repair derived runtime convergence"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn malformed_effectful_composite_reservation_fails_closed() {
    let store = SimEventStore::no_faults(4_012);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("malformed-composite-reservation-test");
    let parent_idempotency = "malformed-effectful-reservation";
    let parent = AtomicCompositeParent {
        tenant: &tenant,
        entity_type: "Parent",
        entity_id: "parent-malformed-reservation",
        action: "CreateChildWithEffect",
        idempotency: parent_idempotency,
        record_event: true,
        agent_ctx: &agent,
    };
    let mut intended = build_composite_event(
        &tenant,
        parent.entity_type,
        parent.entity_id,
        parent.action,
        parent.idempotency,
        &[],
    );
    intended.intent_hash = "exact-intent".to_string();
    let persistence_id =
        ServerState::effectful_composite_reservation_persistence_id(parent, &intended);
    store
        .append(
            &persistence_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: COMPOSITE_EVENT_TYPE.to_string(),
                payload: json!({"schema": "malformed-composite-reservation"}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: persistence_id.clone(),
                },
            }],
        )
        .await
        .expect("seed malformed reserved composite intent");
    let (event_store, _) = state.event_journal().expect("sim event journal");

    let error = state
        .effectful_composite_reservation_exists(&event_store, parent, &intended)
        .await
        .expect_err("reserved composite corruption must block effect replay");
    assert!(
        error
            .to_string()
            .contains("malformed composite reservation")
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_effectful_subwrite_is_durably_content_bound() {
    let store = SimEventStore::no_faults(403);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let mut agent = AgentContext::for_service("composite-effect-retry-test");
    agent.idempotency_key = Some("effectful-parent-operation".to_string());
    let child_id = "child-effect-content-bound";
    let child_pid = format!("default:Child:{child_id}");
    let bootstrap = crate::entity_actor::EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Draft".to_string(),
        timestamp: sim_now(),
        params: json!({}),
        idempotency_key: None,
    };
    store
        .append(
            &child_pid,
            0,
            &[composite_envelope(&child_pid, &bootstrap)
                .expect("encode effectful child bootstrap")],
        )
        .await
        .expect("seed an existing Draft child for the non-Create action");
    let callback = json!({
        "sub_writes": [{
            "entity_type": "Child",
            "entity_id": child_id,
            "action": "CreateWithEffect",
            "params": { "Name": "original effect value" }
        }]
    });
    let metadata = state
        .composite_metadata_for(&tenant, "Parent", "CreateChildWithEffect")
        .expect("load composite metadata")
        .expect("effectful composite metadata");
    let parent_idempotency = composite_parent_idempotency(&agent, &callback);
    let parent = AtomicCompositeParent {
        tenant: &tenant,
        entity_type: "Parent",
        entity_id: "parent-effect-content-bound",
        action: "CreateChildWithEffect",
        idempotency: &parent_idempotency,
        record_event: metadata.record_parent_event,
        agent_ctx: &agent,
    };
    let sub_writes = parse_sub_writes(&callback).expect("parse effectful sub-write");
    let batch_claim = composite_batch_claim(
        parent,
        &prepare_composite_intent_sub_writes(parent, &sub_writes, &metadata),
    )
    .expect("build effectful batch claim");

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-effect-content-bound",
            "CreateChildWithEffect",
            &callback,
            &agent,
        )
        .await
        .expect("effectful composite must commit atomically");
    assert!(
        !store
            .batch_idempotency_committed(&batch_claim)
            .await
            .expect("read effectful batch claim"),
        "non-durable post-commit effects must use actor idempotency instead of an atomic batch claim"
    );
    let journal_len = store.dump_journal(&child_pid).len();

    let error = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-effect-content-bound",
            "CreateChildWithEffect",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": child_id,
                    "action": "CreateWithEffect",
                    "params": { "Name": "different effect value" }
                }]
            }),
            &agent,
        )
        .await
        .expect_err("effectful retry with different content must conflict");
    assert!(error.to_string().contains("different intent"));
    assert_eq!(store.dump_journal(&child_pid).len(), journal_len);
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn delayed_composite_batch_cannot_borrow_reactivated_key_epoch() {
    let store = SimEventStore::no_faults(299);
    let state = composite_registry_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let child_live_table = state
        .registry
        .read()
        .expect("registry lock")
        .get_table_live(&tenant, "Child")
        .expect("Child table");
    let original_table = child_live_table.read().expect("table lock").clone();
    let signature_a = crate::key_index::declared_key_set_signature(&original_table.keys);
    let old_epoch = store
        .activate_key_index_contract(tenant.as_str(), "Child", &signature_a, false)
        .await
        .expect("activate original Child contract");
    store
        .mark_key_index_backfilled(tenant.as_str(), "Child", &signature_a)
        .await
        .expect("publish original Child readiness");
    child_live_table
        .write()
        .expect("table lock")
        .key_contract_activation_epoch = old_epoch;

    let callback_params = json!({
        "sub_writes": [{
            "entity_type": "Child",
            "entity_id": "child-stale-composite",
            "action": "Create",
            "params": { "Name": "must-not-resurrect" }
        }]
    });
    let agent = AgentContext::for_service("composite-epoch-test");
    let pause = store.inject_precommit_batch_pause();
    let composite_future = state.apply_composite_integration_result(
        &tenant,
        "Parent",
        "parent-stale-composite",
        "CreateChild",
        &callback_params,
        &agent,
    );
    tokio::pin!(composite_future);
    tokio::select! {
        result = &mut composite_future => panic!("composite crossed pre-commit barrier: {result:?}"),
        () = pause.wait_until_reached() => {}
    }

    let signature_none = crate::key_index::declared_key_set_signature(&[]);
    let empty_epoch = store
        .activate_key_index_contract(tenant.as_str(), "Child", &signature_none, true)
        .await
        .expect("activate empty Child contract");
    let mut empty_table = original_table.clone();
    empty_table.keys.clear();
    empty_table.key_contract_activation_epoch = empty_epoch;
    *child_live_table.write().expect("table lock") = empty_table;

    let current_epoch = store
        .activate_key_index_contract(tenant.as_str(), "Child", &signature_a, false)
        .await
        .expect("reactivate Child contract");
    let mut current_table = original_table;
    current_table.key_contract_activation_epoch = current_epoch;
    *child_live_table.write().expect("table lock") = current_table;

    pause.resume();
    let error = composite_future
        .await
        .expect_err("staged old-epoch composite must reject atomically");
    assert!(error.to_string().contains("activation is stale"));
    assert!(
        store
            .dump_journal("default:Parent:parent-stale-composite")
            .is_empty()
    );
    assert!(
        store
            .dump_journal("default:Child:child-stale-composite")
            .is_empty()
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_parent_audit_respects_the_raw_replay_tail_budget() {
    let store = SimEventStore::no_faults(296);
    let parent_pid = "default:Parent:parent-at-budget";
    let events = (0..crate::entity_actor::types::MAX_EVENTS_SINCE_SNAPSHOT)
        .map(|_| {
            let event = crate::entity_actor::EntityEvent {
                action: "CreateChild".to_string(),
                from_status: "Active".to_string(),
                to_status: "Active".to_string(),
                timestamp: sim_now(),
                params: json!({}),
                idempotency_key: None,
            };
            PersistenceEnvelope {
                sequence_nr: 0,
                event_type: event.action.clone(),
                payload: serde_json::to_value(event).expect("serialize parent event"),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: parent_pid.to_string(),
                },
            }
        })
        .collect::<Vec<_>>();
    store
        .append(parent_pid, 0, &events)
        .await
        .expect("seed parent at raw replay-tail cap");

    let state = composite_test_state_with_store(store.clone());
    let error = state
        .apply_composite_integration_result(
            &TenantId::default(),
            "Parent",
            "parent-at-budget",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": "child-must-not-append",
                    "action": "Create",
                    "params": {"Name": "blocked by parent budget"}
                }]
            }),
            &AgentContext::for_service("composite-test"),
        )
        .await
        .expect_err("parent audit at the cap must reject the whole composite")
        .to_string();

    assert!(error.contains("parent audit would exceed the event budget"));
    assert_eq!(
        store.dump_journal(parent_pid).len(),
        crate::entity_actor::types::MAX_EVENTS_SINCE_SNAPSHOT
    );
    assert!(
        store
            .dump_journal("default:Child:child-must-not-append")
            .is_empty(),
        "the atomic child write must not commit after the parent budget rejects"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_first_write_materializes_snapshot_only_counter_for_restart() {
    let store = SimEventStore::no_faults(288);
    let tenant = TenantId::default();
    let child_id = "snapshot-only-composite-child";
    let child_pid = format!("default:Child:{child_id}");
    let snapshot = serde_json::to_vec(&json!({
        "entity_type": "Child",
        "entity_id": child_id,
        "status": "Draft",
        "item_count": 0,
        "counters": {"revision": 10},
        "booleans": {},
        "lists": {},
        "fields": {
            "Id": "legacy-wrong-child-id",
            "Name": "snapshot baseline",
            "Status": "LegacyWrongStatus"
        },
        "events": [],
        "total_event_count": 10,
        "events_since_snapshot": 0,
        "last_snapshot_sequence_nr": 5,
        "sequence_nr": 5,
        "processed_idempotency_keys": {}
    }))
    .expect("serialize snapshot-only composite target");
    store
        .save_snapshot(&child_pid, 5, &snapshot)
        .await
        .expect("seed snapshot-only composite target");

    let state = composite_test_state_with_store(store.clone());
    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "snapshot-only-composite-parent",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": child_id,
                    "action": "Create",
                    "params": {"Name": "after composite"}
                }]
            }),
            &AgentContext::for_service("composite-test"),
        )
        .await
        .expect("apply composite write to snapshot-only target");

    let journal = store.dump_journal(&child_pid);
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Temper.Internal.StateMaterialization.v1", "Create"]
    );
    assert_eq!(journal[0].payload["state"]["fields"]["Id"], child_id);
    assert_eq!(journal[0].payload["state"]["fields"]["Status"], "Draft");
    let restarted = composite_test_state_with_store(store);
    let child = restarted
        .get_tenant_entity_state(&tenant, "Child", child_id)
        .await
        .expect("restart snapshot-only composite target");
    assert_eq!(child.state.counters.get("revision"), Some(&11));
    assert_eq!(
        child.state.fields.get("Name"),
        Some(&json!("after composite"))
    );
    assert_eq!(child.state.sequence_nr, 2);
    assert_eq!(child.state.fields["Id"], child_id);
    assert_eq!(child.state.fields["Status"], "Active");
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn snapshot_only_parent_composite_restart_does_not_fabricate_created_event() {
    let store = SimEventStore::no_faults(290);
    let tenant = TenantId::default();
    let parent_id = "snapshot-only-composite-parent";
    let parent_pid = format!("default:Parent:{parent_id}");
    let snapshot = serde_json::to_vec(&json!({
        "entity_type": "Parent",
        "entity_id": parent_id,
        "status": "Active",
        "item_count": 0,
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": {"Id": parent_id, "Status": "Active"},
        "events": [],
        "total_event_count": 0,
        "events_since_snapshot": 0,
        "last_snapshot_sequence_nr": 5,
        "sequence_nr": 5,
        "processed_idempotency_keys": {}
    }))
    .expect("serialize snapshot-only parent");
    store
        .save_snapshot(&parent_pid, 5, &snapshot)
        .await
        .expect("seed snapshot-only parent");

    let state = composite_test_state_with_store(store.clone());
    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            parent_id,
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": "snapshot-parent-child",
                    "action": "Create",
                    "params": {"Name": "child"}
                }]
            }),
            &AgentContext::for_service("composite-test"),
        )
        .await
        .expect("apply composite against snapshot-only parent");

    assert_eq!(
        store
            .dump_journal(&parent_pid)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Temper.Internal.StateMaterialization.v1",
            COMPOSITE_EVENT_TYPE
        ],
        "reloading a materialized parent with only composite audit history must not bootstrap Created"
    );
    let restarted = composite_test_state_with_store(store.clone());
    let parent = restarted
        .get_tenant_entity_state(&tenant, "Parent", parent_id)
        .await
        .expect("restart snapshot-only composite parent");
    assert_eq!(parent.state.sequence_nr, 2);
    assert_eq!(parent.state.total_event_count, 0);
    assert_eq!(
        store.dump_journal(&parent_pid).len(),
        2,
        "a second restart must not fabricate a domain Created event"
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
async fn composite_atomic_batch_allows_existing_sub_write_to_delete_target() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
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
    let child_name_hash = crate::key_index::canonical_key_hash(
        "child_name",
        &["Name".to_string()],
        json!({ "Name": "temporary child" })
            .as_object()
            .expect("key fields object"),
    )
    .expect("complete child name key");
    assert_eq!(
        store
            .lookup_by_key(tenant.as_str(), "Child", "child_name", &child_name_hash,)
            .await
            .expect("lookup created child key"),
        Some(child_id.to_string()),
        "precondition: normal create owns the declared key"
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
    assert_eq!(
        store
            .lookup_by_key(tenant.as_str(), "Child", "child_name", &child_name_hash,)
            .await
            .expect("lookup key after composite delete"),
        None,
        "composite delete must release declared-key ownership atomically"
    );

    let child_journal = store.dump_journal(&format!("default:Child:{child_id}"));
    assert_eq!(
        child_journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Create", "Delete"]
    );
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

#[cfg(feature = "sim")]
#[tokio::test]
async fn concurrent_exact_composite_retries_converge_on_the_durable_claim() {
    let inner = SimEventStore::no_faults(406);
    let store = PauseFirstAtomicStableLoadStore::new(inner.clone());
    let state = composite_test_state_with_paused_atomic_load_store(store.clone());
    let tenant = TenantId::default();
    let mut agent = AgentContext::for_service("concurrent-exact-composite-test");
    agent.idempotency_key = Some("concurrent-exact-parent-key".to_string());
    let callback = json!({
        "sub_writes": [{
            "entity_type": "Child",
            "entity_id": "concurrent-exact-child",
            "action": "Create",
            "params": {"Name": "committed exactly once"}
        }]
    });

    let paused_state = state.clone();
    let paused_tenant = tenant.clone();
    let paused_agent = agent.clone();
    let paused_callback = callback.clone();
    let paused = tokio::spawn(async move {
        paused_state
            .apply_composite_integration_result(
                &paused_tenant,
                "Parent",
                "concurrent-exact-parent",
                "CreateChild",
                &paused_callback,
                &paused_agent,
            )
            .await
    });
    store.wait_until_first_atomic_load_is_paused().await;

    let winner_state = state.clone();
    let winner_tenant = tenant.clone();
    let winner_agent = agent.clone();
    let winner_callback = callback.clone();
    let winner = tokio::spawn(async move {
        winner_state
            .apply_composite_integration_result(
                &winner_tenant,
                "Parent",
                "concurrent-exact-parent",
                "CreateChild",
                &winner_callback,
                &winner_agent,
            )
            .await
    });
    // Without per-claim serialization the peer reaches durable commit while
    // this callback is paused after preflight. With serialization it remains
    // queued; the bounded yields merely let either deterministic schedule make
    // progress before the captured source resumes.
    for _ in 0..256 {
        if !inner
            .dump_journal("default:Child:concurrent-exact-child")
            .is_empty()
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    store.resume_first_atomic_load();
    let winner = winner
        .await
        .expect("concurrent exact callback should join")
        .expect("concurrent exact callback should commit or replay");
    assert!(winner);
    let replay = paused
        .await
        .expect("paused exact callback should join")
        .expect("paused exact callback must replay the committed durable claim");
    assert!(replay);

    assert_eq!(
        inner
            .dump_journal("default:Child:concurrent-exact-child")
            .iter()
            .filter(|event| event.event_type == "Create")
            .count(),
        1,
        "both successful callbacks must converge on one durable child transition"
    );
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
