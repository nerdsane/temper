use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sha1::Digest as _;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ClaimSchemaVerification, ClaimSchemaVerificationOutcome,
    SchemaBundleRecord, SchemaDeploymentStore, SchemaExecutionPin, SchemaOperationIdentity,
    SchemaScope, SchemaScopeKind, SchemaVerificationReceipt, SubmitSchemaBundle,
};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

use crate::events::EntityStateChange;
use crate::storage::StorageStack;

#[path = "router_test/scoped_schema_pin_test.rs"]
mod scoped_schema_pin;

fn test_security_context() -> temper_authz::SecurityContext {
    temper_authz::SecurityContext {
        principal: temper_authz::Principal {
            id: "test-customer".to_string(),
            kind: temper_authz::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "router-test".to_string(),
    }
}

fn claimed_admin_security_context() -> temper_authz::SecurityContext {
    temper_authz::SecurityContext {
        principal: temper_authz::Principal {
            id: "claimed-admin".to_string(),
            kind: temper_authz::PrincipalKind::Admin,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "router-admin-side-channel-test".to_string(),
    }
}

async fn authenticate_test_request(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if request
        .extensions()
        .get::<temper_authz::AuthenticatedRequestContext>()
        .is_none()
    {
        let tenant = request
            .headers()
            .get("x-tenant-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .map(str::trim)
            .map(TenantId::try_new)
            .transpose()
            .expect("test tenant header should be valid")
            .unwrap_or_default();
        request
            .extensions_mut()
            .insert(temper_authz::AuthenticatedRequestContext::new(
                tenant,
                test_security_context(),
            ));
    }
    next.run(request).await
}

fn authenticated_router(state: ServerState) -> Router {
    if state
        .authz
        .get_tenant_policy_text(TenantId::default().as_str())
        .is_none()
    {
        state
            .authz
            .reload_tenant_policies(
                TenantId::default().as_str(),
                "permit(principal, action, resource);",
            )
            .expect("functional router tests should install an explicit policy");
    }
    super::build_router(state).layer(axum::middleware::from_fn(authenticate_test_request))
}

fn git_blob_id(body: &[u8]) -> String {
    let mut hasher = sha1::Sha1::new();
    hasher.update(format!("blob {}\0", body.len()).as_bytes());
    hasher.update(body);
    format!("{:x}", hasher.finalize())
}

fn counted_body(bytes: &'static [u8], polls: Arc<AtomicUsize>) -> Body {
    Body::from_stream(futures_util::stream::once(async move {
        polls.fetch_add(1, Ordering::SeqCst);
        Ok::<_, std::io::Error>(bytes::Bytes::from_static(bytes))
    }))
}

fn test_state() -> ServerState {
    let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test");
    ServerState::new(system, csdl, csdl_xml.to_string())
}

fn test_state_with_ioa() -> ServerState {
    let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-ioa");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Order".to_string(), order_ioa.to_string());
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap()
}

fn test_state_with_active_task_schema() -> ServerState {
    test_state_with_active_task_schema_and_ioa(include_str!(
        "../../../test-fixtures/specs/order.ioa.toml"
    ))
}

fn test_state_with_active_task_schema_and_ioa(order_ioa: &str) -> ServerState {
    let state = test_state_with_ioa();
    let global_csdl = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    let scoped_csdl = global_csdl.replace("Temper.Example", "Temper.ScopedExample");
    let parsed = parse_csdl(&scoped_csdl).expect("scoped CSDL fixture");
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-router".to_string(),
    };
    let digest = format!("sha256:{}", "a".repeat(64));
    {
        let mut registry = state.registry.write().expect("registry lock");
        registry
            .stage_scoped_bundle(
                TenantId::default(),
                scope.clone(),
                digest.clone(),
                parsed,
                scoped_csdl,
                &[("Order", order_ioa)],
            )
            .expect("stage scoped bundle");
        registry
            .activate_scoped_bundle(&TenantId::default(), &scope, &digest, None)
            .expect("activate scoped bundle");
    }
    state
}

async fn test_state_with_durable_active_task_schema() -> (ServerState, SimEventStore) {
    test_state_with_durable_active_task_schema_and_ioa(include_str!(
        "../../../test-fixtures/specs/order.ioa.toml"
    ))
    .await
}

async fn test_state_with_durable_active_task_schema_and_ioa(
    order_ioa: &str,
) -> (ServerState, SimEventStore) {
    let mut state = test_state_with_active_task_schema_and_ioa(order_ioa);
    let store = SimEventStore::no_faults(1_114);
    persist_active_task_schema(&store, order_ioa).await;
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    (state, store)
}

async fn persist_active_task_schema(store: &impl SchemaDeploymentStore, order_ioa: &str) {
    persist_task_schema_bundle(
        store,
        order_ioa,
        &format!("sha256:{}", "a".repeat(64)),
        None,
        "initial",
    )
    .await;
}

async fn persist_task_schema_bundle(
    store: &impl SchemaDeploymentStore,
    order_ioa: &str,
    digest: &str,
    predecessor: Option<&str>,
    operation_tag: &str,
) {
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-router".into(),
    };
    persist_task_schema_bundle_in_scope(
        store,
        order_ioa,
        digest,
        predecessor,
        operation_tag,
        &scope,
    )
    .await;
}

async fn persist_task_schema_bundle_in_scope(
    store: &impl SchemaDeploymentStore,
    order_ioa: &str,
    digest: &str,
    predecessor: Option<&str>,
    operation_tag: &str,
    scope: &SchemaScope,
) {
    let scoped_csdl = include_str!("../../../test-fixtures/specs/model.csdl.xml")
        .replace("Temper.Example", "Temper.ScopedExample");
    store
        .submit_schema_bundle(SubmitSchemaBundle {
            bundle: SchemaBundleRecord {
                tenant: TenantId::default().to_string(),
                scope: scope.clone(),
                digest: digest.to_string(),
                predecessor_digest: predecessor.map(str::to_string),
                canonical_csdl: scoped_csdl,
                canonical_ioa: std::collections::BTreeMap::from([(
                    "Order".into(),
                    order_ioa.into(),
                )]),
                cedar_policies: std::collections::BTreeMap::new(),
                wasm_module_digests: std::collections::BTreeMap::new(),
                migration_module_name: None,
                migration_module_digest: None,
                migration_abi_version: None,
                canonical_budgets: "{}".into(),
            },
            idempotency_key: format!("{operation_tag}-submit"),
            request_digest: format!("sha256:{}", "1".repeat(64)),
            request_id: format!("{operation_tag}-submit"),
        })
        .await
        .unwrap();
    let claimed = store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: TenantId::default().to_string(),
            scope: scope.clone(),
            bundle_digest: digest.to_string(),
            logical_now: 1,
            lease_expires_at: 2,
            operation: SchemaOperationIdentity {
                idempotency_key: format!("{operation_tag}-verify"),
                request_digest: format!("sha256:{}", "2".repeat(64)),
                request_id: format!("{operation_tag}-verify"),
            },
        })
        .await
        .unwrap();
    let fence = match claimed {
        ClaimSchemaVerificationOutcome::Claimed(record)
        | ClaimSchemaVerificationOutcome::Replayed(record) => record.fence,
    };
    let verified = store
        .finish_schema_verification(
            TenantId::default().as_str(),
            scope,
            digest,
            fence,
            SchemaVerificationReceipt {
                id: format!("{operation_tag}-verification"),
                verifier_version: "test/v1".into(),
                input_digest: format!("sha256:{}", "3".repeat(64)),
                passed: true,
            },
        )
        .await
        .unwrap();
    store
        .activate_schema_bundle(ActivateSchemaBundle {
            tenant: TenantId::default().to_string(),
            scope: scope.clone(),
            bundle_digest: digest.to_string(),
            expected_predecessor: predecessor.map(str::to_string),
            expected_fence: verified.fence,
            verification_receipt_id: format!("{operation_tag}-verification"),
            stream_publication_fence: None,
            operation: SchemaOperationIdentity {
                idempotency_key: format!("{operation_tag}-activate"),
                request_digest: format!("sha256:{}", "4".repeat(64)),
                request_id: format!("{operation_tag}-activate"),
            },
        })
        .await
        .unwrap();
}

fn test_state_with_order_and_payment_ioa() -> ServerState {
    let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-ioa-order-payment");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Order".to_string(), order_ioa.to_string());
    // For navigation tests we only need entity creation/read, so reuse the same minimal IOA.
    specs.insert("Payment".to_string(), order_ioa.to_string());
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap()
}

fn test_state_with_typed_references() -> ServerState {
    let csdl_xml = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
 <edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
  <EntityType Name="Workspace"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/><Property Name="Status" Type="Edm.String"/></EntityType>
  <EntityType Name="Document"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/><Property Name="workspace_id" Type="Edm.String"/><Property Name="Status" Type="Edm.String"/></EntityType>
  <EntityContainer Name="Svc"><EntitySet Name="Workspaces" EntityType="Test.Workspace"/><EntitySet Name="Documents" EntityType="Test.Document"/></EntityContainer>
 </Schema></edmx:DataServices>
</edmx:Edmx>"#;
    let workspace = r#"
[automaton]
name = "Workspace"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]
"#;
    let document = r#"
[automaton]
name = "Document"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]
[[state]]
name = "workspace_id"
type = "ref"
entity_type = "Workspace"
initial = ""
[[key]]
name = "workspace"
properties = ["workspace_id"]
entity_id = true
"#;
    let csdl = parse_csdl(csdl_xml).unwrap();
    let specs = std::collections::BTreeMap::from([
        ("Workspace".to_string(), workspace.to_string()),
        ("Document".to_string(), document.to_string()),
    ]);
    ServerState::with_specs(
        ActorSystem::new("typed-reference-e2e"),
        csdl,
        csdl_xml.into(),
        specs,
    )
    .unwrap()
}

#[test]
fn contracted_types_use_the_canonical_actor_path() {
    let mut state = test_state_with_typed_references();
    state.actor_backed_types.insert("Document".into());
    state.actor_backed_types.insert("Workspace".into());
    let tenant = TenantId::default();
    assert!(!state.is_pg_actor_backed(&tenant, "Document"));
    assert!(state.is_pg_actor_backed(&tenant, "Workspace"));
}

fn test_state_with_customer_and_order_ioa() -> ServerState {
    let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-ioa-customer-order");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Customer".to_string(), order_ioa.to_string());
    specs.insert("Order".to_string(), order_ioa.to_string());
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap()
}

fn test_state_with_blob_ioa() -> ServerState {
    let csdl_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Git" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Blob">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="RepositoryId" Type="Edm.String" Nullable="false"/>
        <Property Name="Size" Type="Edm.Int64" Nullable="false"/>
        <Property Name="Content" Type="Edm.Binary" Nullable="false"/>
        <Property Name="CanonicalBytes" Type="Edm.Binary" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
        <Property Name="CreatedAt" Type="Edm.DateTimeOffset" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Blobs" EntityType="Temper.Git.Blob"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let blob_ioa = r#"
[automaton]
name = "Blob"
states = ["Durable"]
initial = "Durable"

[[action]]
name = "Create"
kind = "input"
from = ["Durable"]
to = "Durable"
params = ["RepositoryId", "Size", "Content", "CanonicalBytes", "CreatedAt"]
"#;
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-blob-ingest");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Blob".to_string(), blob_ioa.to_string());
    let mut state = ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap();
    state.data_dir = std::env::temp_dir().join("temper-router-blob-tests");
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"permit(principal, action == Action::"create", resource is Blob);"#,
        )
        .expect("install Blob test policy");
    state
}

fn test_state_with_rate_limit_ioa() -> ServerState {
    let csdl_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.RateLimitTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Widget">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="OwnerId" Type="Edm.String" Nullable="false"/>
        <Property Name="Name" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="RateLimit">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="OwnerId" Type="Edm.String" Nullable="false"/>
        <Property Name="ActionClass" Type="Edm.String" Nullable="false"/>
        <Property Name="Tokens" Type="Edm.Int64" Nullable="false"/>
        <Property Name="Capacity" Type="Edm.Int64" Nullable="false"/>
        <Property Name="RefillPerSecond" Type="Edm.Int64" Nullable="false"/>
        <Property Name="LastRefillAt" Type="Edm.DateTimeOffset" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Widgets" EntityType="Temper.RateLimitTest.Widget"/>
        <EntitySet Name="RateLimits" EntityType="Temper.RateLimitTest.RateLimit"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let widget_ioa = r#"
[automaton]
name = "Widget"
states = ["Active"]
initial = "Active"

[[action]]
name = "Create"
kind = "input"
from = ["Active"]
to = "Active"
params = ["OwnerId", "Name"]
"#;
    let rate_limit_ioa = r#"
[automaton]
name = "RateLimit"
states = ["Active"]
initial = "Active"

[[action]]
name = "Create"
kind = "input"
from = ["Active"]
to = "Active"
params = ["OwnerId", "ActionClass", "Tokens", "Capacity", "RefillPerSecond", "LastRefillAt"]

[[action]]
name = "Consume"
kind = "input"
from = ["Active"]
to = "Active"
params = ["Tokens", "LastRefillAt"]
"#;
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-rate-limit");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Widget".to_string(), widget_ioa.to_string());
    specs.insert("RateLimit".to_string(), rate_limit_ioa.to_string());
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap()
}

fn test_state_with_storage_cap_ioa() -> ServerState {
    let csdl_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.StorageCapTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Owner">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="AccountId" Type="Edm.String" Nullable="false"/>
        <Property Name="DisplayName" Type="Edm.String" Nullable="false"/>
        <Property Name="Contact" Type="Edm.String"/>
        <Property Name="StorageCapBytes" Type="Edm.Int64" Nullable="false"/>
        <Property Name="RateLimitTier" Type="Edm.String" Nullable="false"/>
        <Property Name="PublicKey" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Repository">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="OwnerAccountId" Type="Edm.String" Nullable="false"/>
        <Property Name="Name" Type="Edm.String" Nullable="false"/>
        <Property Name="Description" Type="Edm.String"/>
        <Property Name="DefaultBranch" Type="Edm.String" Nullable="false"/>
        <Property Name="Visibility" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Blob">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="RepositoryId" Type="Edm.String" Nullable="false"/>
        <Property Name="Size" Type="Edm.Int64" Nullable="false"/>
        <Property Name="Content" Type="Edm.Binary" Nullable="false"/>
        <Property Name="CanonicalBytes" Type="Edm.Binary" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
        <Property Name="CreatedAt" Type="Edm.DateTimeOffset" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Owners" EntityType="Temper.StorageCapTest.Owner"/>
        <EntitySet Name="Repositories" EntityType="Temper.StorageCapTest.Repository"/>
        <EntitySet Name="Blobs" EntityType="Temper.StorageCapTest.Blob"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let owner_ioa = r#"
[automaton]
name = "Owner"
states = ["Active", "Suspended"]
initial = "Active"

[[action]]
name = "Create"
kind = "input"
from = ["Active"]
to = "Active"
params = ["AccountId", "DisplayName", "Contact", "StorageCapBytes", "RateLimitTier", "PublicKey"]
"#;
    let repository_ioa = r#"
[automaton]
name = "Repository"
states = ["Provisioning", "Active"]
initial = "Provisioning"

[[action]]
name = "Create"
kind = "input"
from = ["Provisioning"]
to = "Provisioning"
params = ["OwnerAccountId", "Name", "Description", "DefaultBranch", "Visibility"]
"#;
    let blob_ioa = r#"
[automaton]
name = "Blob"
states = ["Durable"]
initial = "Durable"

[[action]]
name = "Create"
kind = "input"
from = ["Durable"]
to = "Durable"
params = ["RepositoryId", "Size", "Content", "CanonicalBytes", "CreatedAt"]
"#;
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-storage-cap");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Owner".to_string(), owner_ioa.to_string());
    specs.insert("Repository".to_string(), repository_ioa.to_string());
    specs.insert("Blob".to_string(), blob_ioa.to_string());
    let mut state = ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap();
    state.data_dir = std::env::temp_dir().join("temper-router-storage-cap-tests");
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("install commons storage test policy");
    state
}

fn test_state_with_account_verification_ioa() -> ServerState {
    let csdl_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.AccountVerificationTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Owner">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="AccountId" Type="Edm.String" Nullable="false"/>
        <Property Name="DisplayName" Type="Edm.String" Nullable="false"/>
        <Property Name="Contact" Type="Edm.String"/>
        <Property Name="StorageCapBytes" Type="Edm.Int64" Nullable="false"/>
        <Property Name="RateLimitTier" Type="Edm.String" Nullable="false"/>
        <Property Name="VerificationProvider" Type="Edm.String"/>
        <Property Name="VerificationSubject" Type="Edm.String"/>
        <Property Name="VerifiedAt" Type="Edm.DateTimeOffset"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Repository">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="OwnerAccountId" Type="Edm.String" Nullable="false"/>
        <Property Name="Name" Type="Edm.String" Nullable="false"/>
        <Property Name="Description" Type="Edm.String"/>
        <Property Name="DefaultBranch" Type="Edm.String" Nullable="false"/>
        <Property Name="Visibility" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Owners" EntityType="Temper.AccountVerificationTest.Owner"/>
        <EntitySet Name="Repositories" EntityType="Temper.AccountVerificationTest.Repository"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let owner_ioa = r#"
[automaton]
name = "Owner"
states = ["PendingVerification", "Verified", "Suspended"]
initial = "PendingVerification"

[[action]]
name = "Create"
kind = "input"
from = ["PendingVerification"]
to = "PendingVerification"
params = ["AccountId", "DisplayName", "Contact", "StorageCapBytes", "RateLimitTier", "VerificationProvider", "VerificationSubject"]

[[action]]
name = "MarkVerified"
kind = "input"
from = ["PendingVerification", "Verified"]
to = "Verified"
params = ["VerificationProvider", "VerificationSubject", "VerifiedAt"]
"#;
    let repository_ioa = r#"
[automaton]
name = "Repository"
states = ["Provisioning", "Active"]
initial = "Provisioning"

[[action]]
name = "Create"
kind = "input"
from = ["Provisioning"]
to = "Provisioning"
params = ["OwnerAccountId", "Name", "Description", "DefaultBranch", "Visibility"]
"#;
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-account-verification");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Owner".to_string(), owner_ioa.to_string());
    specs.insert("Repository".to_string(), repository_ioa.to_string());
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap()
}

fn test_state_with_owner_app_ioa() -> ServerState {
    let csdl_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.AppNameTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Owner">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="AccountId" Type="Edm.String" Nullable="false"/>
        <Property Name="DisplayName" Type="Edm.String" Nullable="false"/>
        <Property Name="Contact" Type="Edm.String"/>
        <Property Name="StorageCapBytes" Type="Edm.Int64" Nullable="false"/>
        <Property Name="RateLimitTier" Type="Edm.String" Nullable="false"/>
        <Property Name="VerifiedAt" Type="Edm.DateTimeOffset"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="App">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="OwnerId" Type="Edm.String" Nullable="false"/>
        <Property Name="Name" Type="Edm.String" Nullable="false"/>
        <Property Name="RepositoryId" Type="Edm.String"/>
        <Property Name="LatestVersionHash" Type="Edm.String"/>
        <Property Name="Exports" Type="Edm.String"/>
        <Property Name="Description" Type="Edm.String"/>
        <Property Name="Visibility" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Owners" EntityType="Temper.AppNameTest.Owner"/>
        <EntitySet Name="Apps" EntityType="Temper.AppNameTest.App"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let owner_ioa = r#"
[automaton]
name = "Owner"
states = ["Verified"]
initial = "Verified"

[[action]]
name = "Create"
kind = "input"
from = ["Verified"]
to = "Verified"
params = ["AccountId", "DisplayName", "Contact", "StorageCapBytes", "RateLimitTier", "VerifiedAt"]
"#;
    let app_ioa = r#"
[automaton]
name = "App"
states = ["Active"]
initial = "Active"

[[action]]
name = "Create"
kind = "input"
from = ["Active"]
to = "Active"
params = ["OwnerId", "Name", "RepositoryId", "LatestVersionHash", "Exports", "Description", "Visibility"]
"#;
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-owner-app-uniqueness");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Owner".to_string(), owner_ioa.to_string());
    specs.insert("App".to_string(), app_ioa.to_string());
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap()
}

#[test]
fn action_bridge_template_renders_route_params() {
    let params = std::collections::BTreeMap::from([
        ("owner".to_string(), "acme".to_string()),
        ("repo".to_string(), "widgets".to_string()),
    ]);

    assert_eq!(
        render_action_bridge_template("rp-{owner}-{repo}", &params).unwrap(),
        "rp-acme-widgets"
    );
    assert!(render_action_bridge_template("rp-{missing}", &params).is_err());
}

#[tokio::test]
async fn git_receive_pack_bridge_response_uses_pkt_line_report() {
    let response = git_receive_pack_response(&["refs/heads/main".to_string()], false, None);
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(
        std::str::from_utf8(&body).unwrap(),
        "000eunpack ok\n0017ok refs/heads/main\n0000"
    );
}

const DATA_ONLY_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.DataOnly" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="LogEntry">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
        <Property Name="Body" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="LogEntries" EntityType="Temper.DataOnly.LogEntry"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const DATA_ONLY_IOA: &str = r#"
[automaton]
name = "LogEntry"
states = ["Recorded"]
initial = "Recorded"

[[state]]
name = "Body"
type = "string"
initial = ""
"#;

fn test_state_with_data_only_ioa() -> ServerState {
    let csdl = parse_csdl(DATA_ONLY_CSDL).unwrap();
    let system = ActorSystem::new("test-data-only-fast-path");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("LogEntry".to_string(), DATA_ONLY_IOA.to_string());
    ServerState::with_specs(system, csdl, DATA_ONLY_CSDL.to_string(), specs).unwrap()
}

async fn test_state_with_data_only_ioa_and_sim() -> (ServerState, SimEventStore) {
    let mut state = test_state_with_data_only_ioa();
    let store = SimEventStore::no_faults(1_115);
    persist_active_task_schema(&store, DATA_ONLY_IOA).await;
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    (state, store)
}

async fn test_state_with_data_only_ioa_and_turso() -> ServerState {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let db_url = format!(
        "file:/tmp/temper-data-only-fast-path-test-{}-{}.db",
        std::process::id(),
        id
    );
    let _ = std::fs::remove_file(db_url.strip_prefix("file:").unwrap_or(&db_url));
    let turso = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = test_state_with_data_only_ioa();
    state.set_storage_stack(StorageStack::from_turso(turso));
    state
}

#[tokio::test]
async fn test_service_document() {
    let app = authenticated_router(test_state());
    let response = app
        .oneshot(Request::get("/tdata").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["value"].is_array());
    assert_eq!(json["@odata.context"], "$metadata");
}

#[tokio::test]
async fn protected_kernel_route_rejects_forged_headers_without_typed_context() {
    let response = super::build_router(test_state())
        .oneshot(
            Request::get("/tdata/Orders")
                .header("x-tenant-id", "victim")
                .header("x-temper-principal-kind", "admin")
                .header("x-temper-principal-id", "attacker")
                .header("x-temper-agent-role", "supervisor")
                .header("x-temper-principal-scopes", "root")
                .header("x-temper-attr-owner", "*")
                .header("x-temper-action-context", "forged")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should run");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn typed_admin_kind_does_not_bypass_governed_mutation_cedar() {
    let mut request = Request::post("/tdata/Orders")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"Id":"claimed-admin-order"}"#))
        .expect("request should build");
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            claimed_admin_security_context(),
        ));
    let response = super::build_router(test_state_with_ioa())
        .oneshot(request)
        .await
        .expect("request should run");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn hints_require_typed_authentication_and_are_tenant_scoped() {
    let state = test_state();
    state.enrich_metadata(&TenantId::default(), "Submit", "default hint");
    state.enrich_metadata(&TenantId::new("tenant-b"), "Approve", "tenant-b hint");

    let unauthenticated = super::build_router(state.clone())
        .oneshot(
            Request::get("/tdata/$hints")
                .body(Body::empty())
                .expect("unauthenticated hints request"),
        )
        .await
        .expect("unauthenticated hints response");
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let response = authenticated_router(state)
        .oneshot(
            Request::get("/tdata/$hints")
                .body(Body::empty())
                .expect("authenticated hints request"),
        )
        .await
        .expect("authenticated hints response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("hints body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("hints JSON");
    assert_eq!(body["Submit"], "default hint");
    assert!(body.get("Approve").is_none());
}

#[tokio::test]
async fn test_metadata_endpoint() {
    let app = authenticated_router(test_state());
    let response = app
        .oneshot(
            Request::get("/tdata/$metadata")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response.headers().get("Content-Type").unwrap();
    assert_eq!(content_type, "application/xml");
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(body_str.contains("edmx:Edmx"));
    assert!(body_str.contains("Temper.Example"));
}

#[tokio::test]
async fn task_scoped_metadata_and_entity_io_use_the_same_immutable_pin() {
    let (state, _store) = test_state_with_durable_active_task_schema().await;
    let app = authenticated_router(state);
    let service_document = app
        .clone()
        .oneshot(
            Request::get("/tdata")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(service_document.status(), StatusCode::OK);
    let service_document_body = axum::body::to_bytes(service_document.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let service_document_json: serde_json::Value =
        serde_json::from_slice(&service_document_body).unwrap();
    assert!(
        service_document_json["value"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "Orders")
    );
    let metadata = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(metadata.status(), StatusCode::OK);
    let metadata_body = axum::body::to_bytes(metadata.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(
        std::str::from_utf8(&metadata_body)
            .unwrap()
            .contains("Temper.ScopedExample")
    );

    let create = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::from(r#"{"id":"scoped-order"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);

    let second_create = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::from(r#"{"id":"scoped-order-2"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second_create.status(), StatusCode::CREATED);

    let collection = app
        .clone()
        .oneshot(
            Request::get("/tdata/Orders?$top=1&$count=true")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(collection.status(), StatusCode::OK);
    let collection_body = axum::body::to_bytes(collection.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let collection_json: serde_json::Value = serde_json::from_slice(&collection_body).unwrap();
    assert_eq!(collection_json["@odata.count"], 2);
    assert_eq!(collection_json["value"].as_array().unwrap().len(), 1);

    let patch = app
        .clone()
        .oneshot(
            Request::patch("/tdata/Orders('scoped-order')")
                .header("Content-Type", "application/json")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::from(r#"{"Notes":"scoped-patch"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(patch.status(), StatusCode::OK);

    let put = app
        .clone()
        .oneshot(
            Request::put("/tdata/Orders('scoped-order')")
                .header("Content-Type", "application/json")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::from(r#"{"Id":"scoped-order","Notes":"scoped-put"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);

    let get = app
        .clone()
        .oneshot(
            Request::get("/tdata/Orders('scoped-order')")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    let body = axum::body::to_bytes(get.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["fields"]["_temper_schema_pin_v1"]["scope"]["id"],
        "task-router"
    );
    assert_eq!(
        json["fields"]["_temper_schema_pin_v1"]["bundle_digest"],
        format!("sha256:{}", "a".repeat(64))
    );
    assert_eq!(json["fields"]["Notes"], "scoped-put");

    let delete = app
        .oneshot(
            Request::delete("/tdata/Orders('scoped-order')")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn task_scoped_entity_recovers_active_pointer_after_server_restart() {
    let (state, store) = test_state_with_durable_active_task_schema().await;
    let digest = format!("sha256:{}", "a".repeat(64));
    state
        .get_or_create_scoped_entity(
            &TenantId::default(),
            "Order",
            "restart-order",
            serde_json::json!({"Id": "restart-order", "Notes": "durable"}),
            SchemaExecutionPin {
                scope: SchemaScope {
                    kind: SchemaScopeKind::Task,
                    id: "task-router".into(),
                },
                bundle_digest: digest.clone(),
            },
        )
        .await
        .unwrap();
    drop(state);

    let mut restarted = test_state_with_ioa();
    restarted.set_storage_stack(StorageStack::from_sim(store, None));
    assert_eq!(
        restarted.registry.read().unwrap().active_scope_digest(
            &TenantId::default(),
            &SchemaScope {
                kind: SchemaScopeKind::Task,
                id: "task-router".into(),
            },
        ),
        None
    );
    let response = authenticated_router(restarted.clone())
        .oneshot(
            Request::get("/tdata/Orders('restart-order')")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        restarted.registry.read().unwrap().active_scope_digest(
            &TenantId::default(),
            &SchemaScope {
                kind: SchemaScopeKind::Task,
                id: "task-router".into(),
            },
        ),
        Some(digest.as_str())
    );
}

#[tokio::test]
async fn malformed_or_inactive_task_scope_never_falls_back_to_global_metadata() {
    let app = authenticated_router(test_state_with_active_task_schema());
    let incomplete = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .header("x-temper-schema-scope-kind", "task")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(incomplete.status(), StatusCode::BAD_REQUEST);

    let empty = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .header("x-temper-schema-scope-kind", "")
                .header("x-temper-schema-scope-id", "")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);

    let invalid_utf8 = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .header(
                    "x-temper-schema-scope-kind",
                    axum::http::HeaderValue::from_bytes(b"\xff").unwrap(),
                )
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_utf8.status(), StatusCode::BAD_REQUEST);

    let digest_without_scope = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .header(
                    "x-temper-schema-bundle-digest",
                    format!("sha256:{}", "a".repeat(64)),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(digest_without_scope.status(), StatusCode::BAD_REQUEST);

    let malformed_digest = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .header("x-temper-schema-bundle-digest", "sha256:NOT-CANONICAL")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(malformed_digest.status(), StatusCode::BAD_REQUEST);

    let missing_digest = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .header(
                    "x-temper-schema-bundle-digest",
                    format!("sha256:{}", "b".repeat(64)),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_digest.status(), StatusCode::CONFLICT);
    let missing_body = axum::body::to_bytes(missing_digest.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let missing_json: serde_json::Value = serde_json::from_slice(&missing_body).unwrap();
    assert_eq!(missing_json["error"]["code"], "SchemaPinMismatch");

    let inactive = app
        .oneshot(
            Request::get("/tdata/$metadata")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "missing-task")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(inactive.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_entity_set_listing() {
    let app = authenticated_router(test_state());
    let response = app
        .oneshot(Request::get("/tdata/Orders").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["@odata.context"], "$metadata#Orders");
}

#[tokio::test]
async fn test_entity_by_key_not_found() {
    let app = authenticated_router(test_state());
    let response = app
        .oneshot(
            Request::get("/tdata/Orders('abc-123')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Nonexistent entity returns 404 (no transition table = no actor)
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_entity_by_key_found() {
    let app = authenticated_router(test_state_with_ioa());

    // First create an entity via POST
    let create_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "test-1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

    // Now GET the created entity
    let response = app
        .oneshot(
            Request::get("/tdata/Orders('test-1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["@odata.context"], "$metadata#Orders/$entity");
}

#[tokio::test]
async fn typed_reference_create_derives_id_and_rejects_rebind() {
    let app = authenticated_router(test_state_with_typed_references());
    let missing_target = app
        .clone()
        .oneshot(
            Request::post("/tdata/Documents")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"workspace_id":"ws-missing"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_target.status(), StatusCode::CONFLICT);

    let workspace = app
        .clone()
        .oneshot(
            Request::post("/tdata/Workspaces")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id":"ws-1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(workspace.status(), StatusCode::CREATED);

    let create = app
        .clone()
        .oneshot(
            Request::post("/tdata/Documents")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"workspace_id":"ws-1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(create.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let expected_id = crate::key_index::canonical_key_hash(
        "workspace",
        &["workspace_id".to_string()],
        serde_json::json!({"workspace_id":"ws-1"})
            .as_object()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(json["entity_id"], expected_id);

    let rebind = app
        .oneshot(
            Request::patch(format!("/tdata/Documents('{expected_id}')"))
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"workspace_id":"ws-2"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rebind.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(rebind.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "ConstraintViolation");
}

#[tokio::test]
async fn test_unknown_entity_set_returns_404() {
    let app = authenticated_router(test_state());
    let response = app
        .oneshot(
            Request::get("/tdata/NonExistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_post_entity_creation() {
    let app = authenticated_router(test_state_with_ioa());
    let response = app
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"status": "Draft"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn test_post_entity_creation_uses_odata_id_property() {
    let app = authenticated_router(test_state_with_ioa());
    let create_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"Id": "upper-1", "Status": "Draft"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

    let get_response = app
        .oneshot(
            Request::get("/tdata/Orders('upper-1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_response.status(), StatusCode::OK);
}

#[tokio::test]
async fn collection_create_rejects_conflicting_identity_or_lifecycle_aliases() {
    let app = authenticated_router(test_state_with_ioa());

    let conflicting_id = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"id":"order-lower","Id":"order-upper","status":"Draft"}"#,
                ))
                .expect("conflicting ID request"),
        )
        .await
        .expect("conflicting ID response");
    assert_eq!(conflicting_id.status(), StatusCode::BAD_REQUEST);

    let forged_status = app
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id":"order-status","Status":"Shipped"}"#))
                .expect("forged status request"),
        )
        .await
        .expect("forged status response");
    assert_eq!(forged_status.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn collection_create_derives_and_persists_authoritative_compatibility_aliases() {
    let state = test_state_with_ioa();
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"
permit(principal, action == Action::"create", resource is Order)
when {
  resource.id == "order-trusted" &&
  resource.Id == "order-trusted" &&
  resource.status == "Draft" &&
  resource.Status == "Draft"
};
forbid(principal, action == Action::"create", resource is Order)
when { resource has ctx_owner_status };
"#,
        )
        .expect("trusted create policy");
    let app = authenticated_router(state);

    let response = app
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{
                        "id":"order-trusted",
                        "Id":"order-trusted",
                        "status":"Draft",
                        "Status":"Draft",
                        "has_spec":true,
                        "HasSpec":true,
                        "ctx_owner_status":"Privileged",
                        "customer":"Alice"
                    }"#,
                ))
                .expect("trusted create request"),
        )
        .await
        .expect("trusted create response");
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("create response body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("create response JSON");
    assert_eq!(body["fields"]["id"], "order-trusted");
    assert_eq!(body["fields"]["Id"], "order-trusted");
    assert_eq!(body["fields"]["status"], "Draft");
    assert_eq!(body["fields"]["Status"], "Draft");
    assert!(body["fields"].get("ctx_owner_status").is_none());
    assert!(body["fields"].get("has_spec").is_none());
    assert!(body["fields"].get("HasSpec").is_none());
}

#[tokio::test]
async fn test_data_only_entity_create_fast_path_persists_projection_without_actor_spawn() {
    let state = test_state_with_data_only_ioa_and_turso().await;
    let app = authenticated_router(state.clone());

    let create_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/LogEntries")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"Id": "entry-1", "Body": "created through fast path"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);
    let create_body = axum::body::to_bytes(create_response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let create_json: serde_json::Value = serde_json::from_slice(&create_body).unwrap();
    assert_eq!(create_json["status"], "Recorded");
    assert_eq!(create_json["fields"]["Body"], "created through fast path");

    let actor_key = "default:LogEntry:entry-1";
    assert!(
        !state.actor_registry.read().unwrap().contains_key(actor_key),
        "data-only fast path should not hydrate an actor during create"
    );

    let get_response = app
        .oneshot(
            Request::get("/tdata/LogEntries('entry-1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_response.status(), StatusCode::OK);

    let hydrated = state
        .get_tenant_entity_state(&TenantId::default(), "LogEntry", "entry-1")
        .await
        .expect("fast-path entity should replay through actor hydration");
    assert_eq!(hydrated.state.status, "Recorded");
    assert_eq!(hydrated.state.sequence_nr, 1);
    assert_eq!(hydrated.state.fields["Body"], "created through fast path");
    assert!(state.actor_registry.read().unwrap().contains_key(actor_key));
}

#[tokio::test]
async fn test_data_only_create_fast_path_declines_action_bearing_entities() {
    let state = test_state_with_ioa();
    let response = state
        .try_create_data_only_tenant_entity(
            &TenantId::default(),
            "Order",
            "order-fast-path-skip",
            serde_json::json!({"Id": "order-fast-path-skip", "Status": "Draft"}),
        )
        .await
        .unwrap();
    assert!(
        response.is_none(),
        "entities with transition rules must stay on the actor-backed create path"
    );
}

#[tokio::test]
async fn commons_rate_limit_returns_429_per_owner_bucket() {
    let state = test_state_with_rate_limit_ioa();
    state
        .authz
        .reload_tenant_policies("beta", "permit(principal, action, resource);")
        .expect("beta functional test policy should parse");
    state.enable_commons_guardrails("default");
    state.enable_commons_guardrails("beta");
    let app = authenticated_router(state.clone());

    let alice_bucket = ServerState::commons_rate_limit_entity_id("alice", "write");
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/RateLimits")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(format!(
                    r#"{{
                        "Id":"{alice_bucket}",
                        "OwnerId":"alice",
                        "ActionClass":"write",
                        "Tokens":1,
                        "Capacity":1,
                        "RefillPerSecond":0,
                        "LastRefillAt":"2026-05-18T00:00:00Z"
                    }}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let first = app
        .clone()
        .oneshot(
            Request::post("/tdata/Widgets")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"wd-alice-1","OwnerId":"alice","Name":"first"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let exhausted = app
        .clone()
        .oneshot(
            Request::post("/tdata/Widgets")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"wd-alice-2","OwnerId":"alice","Name":"second"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exhausted.status(), StatusCode::TOO_MANY_REQUESTS);

    let mut claimed_admin_request = Request::post("/tdata/Widgets")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"Id":"wd-alice-admin","OwnerId":"alice","Name":"claimed admin"}"#,
        ))
        .expect("claimed-admin request should build");
    claimed_admin_request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            claimed_admin_security_context(),
        ));
    let claimed_admin = app
        .clone()
        .oneshot(claimed_admin_request)
        .await
        .expect("claimed-admin request should run");
    assert_eq!(claimed_admin.status(), StatusCode::TOO_MANY_REQUESTS);

    let bob_bucket = ServerState::commons_rate_limit_entity_id("bob", "write");
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/RateLimits")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "bob")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(format!(
                    r#"{{
                        "Id":"{bob_bucket}",
                        "OwnerId":"bob",
                        "ActionClass":"write",
                        "Tokens":1,
                        "Capacity":1,
                        "RefillPerSecond":0,
                        "LastRefillAt":"2026-05-18T00:00:00Z"
                    }}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let bob_first = app
        .clone()
        .oneshot(
            Request::post("/tdata/Widgets")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "bob")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"wd-bob-1","OwnerId":"bob","Name":"first"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bob_first.status(), StatusCode::CREATED);

    let bucket = state
        .get_tenant_entity_state(&TenantId::default(), "RateLimit", &alice_bucket)
        .await
        .expect("alice bucket should be readable");
    assert_eq!(
        bucket.state.fields.get("Tokens"),
        Some(&serde_json::json!(0))
    );

    let beta_bucket = ServerState::commons_rate_limit_entity_id("alice", "write");
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/RateLimits")
                .header("Content-Type", "application/json")
                .header("X-Tenant-Id", "beta")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(format!(
                    r#"{{
                        "Id":"{beta_bucket}",
                        "OwnerId":"alice",
                        "ActionClass":"write",
                        "Tokens":1,
                        "Capacity":1,
                        "RefillPerSecond":0,
                        "LastRefillAt":"2026-05-18T00:00:00Z"
                    }}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let beta_first = app
        .clone()
        .oneshot(
            Request::post("/tdata/Widgets")
                .header("Content-Type", "application/json")
                .header("X-Tenant-Id", "beta")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"wd-beta-alice-1","OwnerId":"alice","Name":"first"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        beta_first.status(),
        StatusCode::CREATED,
        "default/alice exhaustion must not consume beta/alice's bucket"
    );

    let beta_exhausted = app
        .clone()
        .oneshot(
            Request::post("/tdata/Widgets")
                .header("Content-Type", "application/json")
                .header("X-Tenant-Id", "beta")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"wd-beta-alice-2","OwnerId":"alice","Name":"second"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(beta_exhausted.status(), StatusCode::TOO_MANY_REQUESTS);

    let beta_bucket_state = state
        .get_tenant_entity_state(&TenantId::new("beta"), "RateLimit", &beta_bucket)
        .await
        .expect("beta alice bucket should be readable");
    assert_eq!(
        beta_bucket_state.state.fields.get("Tokens"),
        Some(&serde_json::json!(0))
    );

    let beta_widget = state
        .get_tenant_entity_state(&TenantId::new("beta"), "Widget", "wd-beta-alice-1")
        .await
        .expect("beta widget should be readable");
    assert_eq!(
        beta_widget.state.fields.get("OwnerId"),
        Some(&serde_json::json!("alice"))
    );
    assert!(!state.entity_exists(&TenantId::default(), "Widget", "wd-beta-alice-1"));
}

#[tokio::test]
async fn test_blob_ingest_raw_route_streams_body_without_path_param() {
    let app = authenticated_router(test_state_with_blob_ioa());
    let response = app
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "3")
                .header("X-Expected-Object-Id", git_blob_id(b"abc"))
                .header("X-Repository-Id", "rp-acme-demo")
                .body(Body::from("abc"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["fields"]["Id"],
        "f2ba8f84ab5c1bce84a7b441cb1959cfc7093b7f"
    );
    assert_eq!(json["fields"]["RepositoryId"], "rp-acme-demo");
    assert_eq!(json["fields"]["Size"], 3);
}

#[tokio::test]
async fn test_blob_ingest_raw_applies_cedar_create_policy() {
    let state = test_state_with_blob_ioa();
    let tenant = TenantId::default();
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            r#"permit(principal, action == Action::"read", resource is Blob);"#,
        )
        .expect("install Cedar policy");

    let response = authenticated_router(state.clone())
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "3")
                .header("X-Expected-Object-Id", git_blob_id(b"abc"))
                .header("X-Repository-Id", "rp-acme-demo")
                .header("X-Temper-Principal-Id", "customer-1")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from("abc"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(state.list_entity_ids(&tenant, "Blob").is_empty());
}

#[tokio::test]
async fn commons_storage_cap_blocks_raw_blob_ingest_per_owner() {
    let state = test_state_with_storage_cap_ioa();
    state.enable_commons_guardrails("default");
    let app = authenticated_router(state.clone());

    let alice_owner = app
        .clone()
        .oneshot(
            Request::post("/tdata/Owners")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"alice","AccountId":"alice","DisplayName":"Alice","Contact":"alice@example.test","StorageCapBytes":3,"RateLimitTier":"free","PublicKey":""}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(alice_owner.status(), StatusCode::CREATED);

    let alice_repo = app
        .clone()
        .oneshot(
            Request::post("/tdata/Repositories")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"rp-alice","OwnerAccountId":"alice","Name":"demo","Description":"","DefaultBranch":"refs/heads/main","Visibility":"public"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(alice_repo.status(), StatusCode::CREATED);

    let alice_first = app
        .clone()
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "3")
                .header("X-Expected-Object-Id", git_blob_id(b"abc"))
                .header("X-Repository-Id", "rp-alice")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from("abc"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(alice_first.status(), StatusCode::CREATED);

    let exceeded_body_polls = Arc::new(AtomicUsize::new(0));
    let alice_exceeded = app
        .clone()
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "2")
                .header("X-Expected-Object-Id", git_blob_id(b"de"))
                .header("X-Repository-Id", "rp-alice")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(counted_body(b"de", exceeded_body_polls.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(alice_exceeded.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        exceeded_body_polls.load(Ordering::SeqCst),
        0,
        "over-quota raw bodies must not be polled"
    );

    let bob_owner = app
        .clone()
        .oneshot(
            Request::post("/tdata/Owners")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "bob")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"bob","AccountId":"bob","DisplayName":"Bob","Contact":"bob@example.test","StorageCapBytes":2,"RateLimitTier":"free","PublicKey":""}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bob_owner.status(), StatusCode::CREATED);

    let bob_repo = app
        .clone()
        .oneshot(
            Request::post("/tdata/Repositories")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "bob")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"rp-bob","OwnerAccountId":"bob","Name":"demo","Description":"","DefaultBranch":"refs/heads/main","Visibility":"public"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bob_repo.status(), StatusCode::CREATED);

    let bob_first = app
        .clone()
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "2")
                .header("X-Expected-Object-Id", git_blob_id(b"xy"))
                .header("X-Repository-Id", "rp-bob")
                .header("X-Temper-Principal-Id", "bob")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from("xy"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bob_first.status(), StatusCode::CREATED);

    let tenant = temper_runtime::tenant::TenantId::default();
    let alice_projection = state
        .commons_storage_projection_for_owner(&tenant, "alice")
        .await
        .unwrap()
        .expect("alice owner projection should exist");
    assert_eq!(alice_projection.used_bytes, 3);
    assert_eq!(alice_projection.cap_bytes, 3);

    let bob_projection = state
        .commons_storage_projection_for_owner(&tenant, "bob")
        .await
        .unwrap()
        .expect("bob owner projection should exist");
    assert_eq!(bob_projection.used_bytes, 2);
    assert_eq!(bob_projection.cap_bytes, 2);
    assert_eq!(state.list_entity_ids(&tenant, "Blob").len(), 2);
}

#[tokio::test]
async fn commons_storage_projection_cache_invalidates_after_blob_write() {
    let state = test_state_with_storage_cap_ioa();
    state.enable_commons_guardrails("default");
    let app = authenticated_router(state.clone());

    let owner = app
        .clone()
        .oneshot(
            Request::post("/tdata/Owners")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "carol")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"carol","AccountId":"carol","DisplayName":"Carol","Contact":"carol@example.test","StorageCapBytes":6,"RateLimitTier":"free","PublicKey":""}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(owner.status(), StatusCode::CREATED);

    let repo = app
        .clone()
        .oneshot(
            Request::post("/tdata/Repositories")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "carol")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"rp-carol","OwnerAccountId":"carol","Name":"demo","Description":"","DefaultBranch":"refs/heads/main","Visibility":"public"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(repo.status(), StatusCode::CREATED);

    let tenant = temper_runtime::tenant::TenantId::default();
    let first = app
        .clone()
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "2")
                .header("X-Expected-Object-Id", git_blob_id(b"aa"))
                .header("X-Repository-Id", "rp-carol")
                .header("X-Temper-Principal-Id", "carol")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from("aa"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let cached_projection = state
        .commons_storage_projection_for_owner(&tenant, "carol")
        .await
        .unwrap()
        .expect("carol owner projection should exist");
    assert_eq!(cached_projection.used_bytes, 2);

    let second = app
        .clone()
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "2")
                .header("X-Expected-Object-Id", git_blob_id(b"bb"))
                .header("X-Repository-Id", "rp-carol")
                .header("X-Temper-Principal-Id", "carol")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from("bb"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);

    let refreshed_projection = state
        .commons_storage_projection_for_owner(&tenant, "carol")
        .await
        .unwrap()
        .expect("carol owner projection should still exist");
    assert_eq!(refreshed_projection.used_bytes, 4);
    assert_eq!(refreshed_projection.cap_bytes, 6);
}

#[tokio::test]
async fn commons_storage_reservation_allows_other_writers_and_prevents_overreservation() {
    let mut state = test_state_with_storage_cap_ioa();
    state.raw_blob_ingest_budget = crate::blob_store::BlobIngestBudget::with_limits(
        16,
        1,
        4,
        2,
        crate::blob_store::BlobIngestProgressPolicy::new(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(1),
            1,
        ),
    );
    state.enable_commons_guardrails("default");
    let app = authenticated_router(state.clone());

    let owner = app
        .clone()
        .oneshot(
            Request::post("/tdata/Owners")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "dana")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"dana","AccountId":"dana","DisplayName":"Dana","Contact":"dana@example.test","StorageCapBytes":4,"RateLimitTier":"free","PublicKey":""}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(owner.status(), StatusCode::CREATED);

    let repo = app
        .clone()
        .oneshot(
            Request::post("/tdata/Repositories")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "dana")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"rp-dana","OwnerAccountId":"dana","Name":"demo","Description":"","DefaultBranch":"refs/heads/main","Visibility":"public"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(repo.status(), StatusCode::CREATED);

    let first_app = app.clone();
    let first_body_polls = Arc::new(AtomicUsize::new(0));
    let second_body_polls = Arc::new(AtomicUsize::new(0));
    let first_body_polls_for_request = first_body_polls.clone();
    let slow_body = Body::from_stream(async_stream::stream! {
        first_body_polls_for_request.fetch_add(1, Ordering::SeqCst);
        yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"abcd"));
        std::future::pending::<()>().await;
    });
    let first = tokio::spawn(
        first_app.oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "4")
                .header("X-Expected-Object-Id", git_blob_id(b"abcd"))
                .header("X-Repository-Id", "rp-dana")
                .body(slow_body)
                .unwrap(),
        ),
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while first_body_polls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("slow upload should reach body staging");

    let unrelated_owner = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        app.clone().oneshot(
            Request::post("/tdata/Owners")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"Id":"erin","AccountId":"erin","DisplayName":"Erin","Contact":"erin@example.test","StorageCapBytes":4,"RateLimitTier":"free","PublicKey":""}"#,
                ))
                .unwrap(),
        ),
    )
    .await
    .expect("slow Blob body must not hold the coarse commons lock")
    .unwrap();
    assert_eq!(unrelated_owner.status(), StatusCode::CREATED);

    let second = app
        .clone()
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "4")
                .header("X-Expected-Object-Id", git_blob_id(b"wxyz"))
                .header("X-Repository-Id", "rp-dana")
                .body(counted_body(b"wxyz", second_body_polls.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(second_body_polls.load(Ordering::SeqCst), 0);

    first.abort();
    let _ = first.await;
    let committed = app
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "4")
                .header("X-Expected-Object-Id", git_blob_id(b"abcd"))
                .header("X-Repository-Id", "rp-dana")
                .body(Body::from("abcd"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(committed.status(), StatusCode::CREATED);

    let tenant = temper_runtime::tenant::TenantId::default();
    let projection = state
        .commons_storage_projection_for_owner(&tenant, "dana")
        .await
        .unwrap()
        .expect("dana owner projection should exist");
    assert_eq!(projection.used_bytes, 4);
    assert_eq!(projection.cap_bytes, 4);
    assert_eq!(state.list_entity_ids(&tenant, "Blob").len(), 1);
}

#[tokio::test]
async fn commons_account_verification_blocks_owner_scoped_writes_until_verified() {
    let state = test_state_with_account_verification_ioa();
    state.enable_commons_guardrails("default");
    let app = authenticated_router(state.clone());

    let owner = app
        .clone()
        .oneshot(
            Request::post("/tdata/Owners")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"alice","AccountId":"alice","DisplayName":"Alice","Contact":"alice@example.test","StorageCapBytes":1024,"RateLimitTier":"free","VerificationProvider":"email","VerificationSubject":"alice@example.test"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(owner.status(), StatusCode::CREATED);

    let unverified_repo = app
        .clone()
        .oneshot(
            Request::post("/tdata/Repositories")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"rp-alice-blocked","OwnerAccountId":"alice","Name":"blocked","Description":"","DefaultBranch":"refs/heads/main","Visibility":"public"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unverified_repo.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(unverified_repo.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "AccountVerificationRequired");

    let verify = app
        .clone()
        .oneshot(
            Request::post(
                "/tdata/Owners('alice')/Temper.AccountVerificationTest.MarkVerified",
            )
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Id", "operator")
            .header("X-Temper-Principal-Kind", "admin")
            .body(Body::from(
                r#"{"VerificationProvider":"email","VerificationSubject":"alice@example.test","VerifiedAt":"2026-05-19T00:00:00Z"}"#,
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(verify.status(), StatusCode::OK);

    let verified_owner = state
        .get_tenant_entity_state(
            &temper_runtime::tenant::TenantId::default(),
            "Owner",
            "alice",
        )
        .await
        .expect("verified owner should be readable");
    assert_eq!(verified_owner.state.status, "Verified");
    assert_eq!(
        verified_owner.state.fields.get("VerificationProvider"),
        Some(&serde_json::json!("email"))
    );
    assert_eq!(
        verified_owner.state.fields.get("VerificationSubject"),
        Some(&serde_json::json!("alice@example.test"))
    );
    assert_eq!(
        verified_owner.state.fields.get("VerifiedAt"),
        Some(&serde_json::json!("2026-05-19T00:00:00Z"))
    );

    let verified_repo = app
        .clone()
        .oneshot(
            Request::post("/tdata/Repositories")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"rp-alice-allowed","OwnerAccountId":"alice","Name":"allowed","Description":"","DefaultBranch":"refs/heads/main","Visibility":"public"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(verified_repo.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn commons_app_name_unique_per_owner_on_create_and_patch() {
    let state = test_state_with_owner_app_ioa();
    state.enable_commons_guardrails("default");
    let app = authenticated_router(state);

    let owner = app
        .clone()
        .oneshot(
            Request::post("/tdata/Owners")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"alice","AccountId":"alice","DisplayName":"Alice","Contact":"alice@example.test","StorageCapBytes":1024,"RateLimitTier":"free","VerifiedAt":"2026-05-19T00:00:00Z"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(owner.status(), StatusCode::CREATED);

    let first = app
        .clone()
        .oneshot(
            Request::post("/tdata/Apps")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"app-alice-notes","OwnerId":"alice","Name":"notes","RepositoryId":"rp-a","LatestVersionHash":"h1","Exports":"[]","Description":"","Visibility":"public"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let duplicate_create = app
        .clone()
        .oneshot(
            Request::post("/tdata/Apps")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"app-alice-notes-copy","OwnerId":"alice","Name":"Notes","RepositoryId":"rp-b","LatestVersionHash":"h2","Exports":"[]","Description":"","Visibility":"public"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(duplicate_create.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(duplicate_create.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "AppNameAlreadyExists");

    let second_owner = app
        .clone()
        .oneshot(
            Request::post("/tdata/Owners")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "bob")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"bob","AccountId":"bob","DisplayName":"Bob","Contact":"bob@example.test","StorageCapBytes":1024,"RateLimitTier":"free","VerifiedAt":"2026-05-19T00:00:00Z"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second_owner.status(), StatusCode::CREATED);

    let same_name_other_owner = app
        .clone()
        .oneshot(
            Request::post("/tdata/Apps")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "bob")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"app-bob-notes","OwnerId":"bob","Name":"notes","RepositoryId":"rp-c","LatestVersionHash":"h3","Exports":"[]","Description":"","Visibility":"public"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(same_name_other_owner.status(), StatusCode::CREATED);

    let bob_other = app
        .clone()
        .oneshot(
            Request::post("/tdata/Apps")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "bob")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(
                    r#"{"Id":"app-bob-tasks","OwnerId":"bob","Name":"tasks","RepositoryId":"rp-d","LatestVersionHash":"h4","Exports":"[]","Description":"","Visibility":"public"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bob_other.status(), StatusCode::CREATED);

    let duplicate_patch = app
        .clone()
        .oneshot(
            Request::patch("/tdata/Apps('app-bob-tasks')")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Id", "bob")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from(r#"{"Name":"Notes"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(duplicate_patch.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_post_bound_action() {
    let app = authenticated_router(test_state_with_ioa());
    let response = app
        .oneshot(
            Request::post("/tdata/Orders('abc-123')/Temper.Example.CancelOrder")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .body(Body::from(r#"{"Reason": "changed mind"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "Cancelled");
}

#[tokio::test]
async fn test_odata_version_header() {
    let app = authenticated_router(test_state());
    let response = app
        .oneshot(Request::get("/tdata/Orders").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let odata_version = response.headers().get("OData-Version").unwrap();
    assert_eq!(odata_version, "4.0");
}

#[tokio::test]
async fn test_old_odata_path_returns_404() {
    let app = authenticated_router(test_state());
    let response = app
        .oneshot(Request::get("/odata").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_post_body_used_for_entity_creation() {
    let app = authenticated_router(test_state_with_ioa());

    // Create with specific ID and fields
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "order-42", "customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Verify the body fields were stored
    assert_eq!(json["fields"]["customer"], "Bob");
    assert_eq!(json["fields"]["id"], "order-42");
}

#[tokio::test]
async fn test_entity_set_returns_created_entities() {
    let app = authenticated_router(test_state_with_ioa());

    // Create two entities
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "o1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "o2", "customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // GET the entity set — should return both entities
    let response = app
        .oneshot(Request::get("/tdata/Orders").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let values = json["value"].as_array().unwrap();
    assert_eq!(values.len(), 2);
}

#[tokio::test]
async fn test_patch_updates_entity() {
    let app = authenticated_router(test_state_with_ioa());

    // Create entity
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "p1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // PATCH the entity
    let response = app
        .clone()
        .oneshot(
            Request::patch("/tdata/Orders('p1')")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["fields"]["customer"], "Bob");
}

#[tokio::test]
async fn test_delete_removes_entity() {
    let app = authenticated_router(test_state_with_ioa());

    // Create entity
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "d1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // DELETE
    let response = app
        .clone()
        .oneshot(
            Request::delete("/tdata/Orders('d1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // GET should now return 404
    let response = app
        .oneshot(
            Request::get("/tdata/Orders('d1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_patch_nonexistent_returns_404() {
    let app = authenticated_router(test_state_with_ioa());
    let response = app
        .oneshot(
            Request::patch("/tdata/Orders('nope')")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_returns_404() {
    let app = authenticated_router(test_state_with_ioa());
    let response = app
        .oneshot(
            Request::delete("/tdata/Orders('nope')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_navigation_property_single_entity() {
    let app = authenticated_router(test_state_with_order_and_payment_ioa());

    // Create parent order.
    let order_create = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "ord-nav-1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(order_create.status(), StatusCode::CREATED);

    // Create related payment linked by OrderId.
    let payment_create = app
        .clone()
        .oneshot(
            Request::post("/tdata/Payments")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "pay-nav-1", "OrderId": "ord-nav-1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(payment_create.status(), StatusCode::CREATED);

    let response = app
        .oneshot(
            Request::get("/tdata/Orders('ord-nav-1')/Payment")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["entity_type"], "Payment");
    assert_eq!(json["fields"]["OrderId"], "ord-nav-1");
}

#[tokio::test]
async fn test_collection_navigation_requires_cedar_list_policy() {
    let state = test_state_with_customer_and_order_ioa();
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(&tenant, "Customer", "cust-nav", serde_json::json!({}))
        .await
        .expect("create customer");
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            "ord-nav-child",
            serde_json::json!({"CustomerId": "cust-nav"}),
        )
        .await
        .expect("create order");
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            r#"
                permit(principal, action == Action::"read", resource is Customer);
                permit(principal, action == Action::"read", resource is Order);
            "#,
        )
        .expect("install Cedar policy");

    let response = authenticated_router(state)
        .oneshot(
            Request::get("/tdata/Customers('cust-nav')?$expand=Orders")
                .header("X-Temper-Principal-Id", "customer-1")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_navigation_property_not_found_returns_404() {
    let app = authenticated_router(test_state_with_ioa());
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "ord-nav-missing"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let response = app
        .oneshot(
            Request::get("/tdata/Orders('ord-nav-missing')/DefinitelyMissingNav")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_temper_client_script_served() {
    let app = authenticated_router(test_state());
    let response = app
        .oneshot(
            Request::get("/temper-client.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("Content-Type").unwrap(),
        "application/javascript"
    );
    assert_eq!(
        response.headers().get("Cache-Control").unwrap(),
        "public, max-age=3600"
    );
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(body_str.contains("Temper"));
}

#[tokio::test]
async fn test_temper_client_script_alias_served() {
    let app = authenticated_router(test_state());
    let response = app
        .oneshot(
            Request::get("/static/temper-client.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("Content-Type").unwrap(),
        "application/javascript"
    );
}

#[tokio::test]
async fn test_cors_header_present() {
    let app = authenticated_router(test_state());
    let response = app
        .oneshot(
            Request::get("/tdata/Orders")
                .header("Origin", "http://localhost:5173")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response
            .headers()
            .get("Access-Control-Allow-Origin")
            .unwrap(),
        "*"
    );
}

/// Read SSE data from an axum body stream until `predicate` matches or timeout expires.
async fn collect_sse_frames_until(
    body: Body,
    predicate: impl Fn(&str) -> bool,
    timeout_ms: u64,
) -> String {
    use tokio_stream::StreamExt as _;

    let mut stream = body.into_data_stream();
    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(500), stream.next()).await {
            Ok(Some(Ok(bytes))) => {
                collected.push_str(&String::from_utf8_lossy(&bytes));
                if predicate(&collected) {
                    break;
                }
            }
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => continue, // timeout on this chunk, try again
        }
    }
    collected
}

#[tokio::test]
async fn test_sse_events_endpoint_delivers_state_changes() {
    let state = test_state_with_ioa();
    let event_tx = state.event_tx.clone();
    let app = authenticated_router(state);

    // Connect to SSE endpoint — response should be 200 with text/event-stream.
    let response = app
        .oneshot(
            Request::get("/tdata/$events")
                .header("X-Temper-Principal-Kind", "admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/event-stream"),
    );

    // Send a state change event on the broadcast channel.
    let _ = event_tx.send(EntityStateChange {
        seq: 1,
        entity_type: "Order".into(),
        entity_id: "o-sse-1".into(),
        action: "SubmitOrder".into(),
        status: "Submitted".into(),
        tenant: "default".into(),
        agent_id: Some("test-agent".into()),
        session_id: None,
        intent: None,
        observation_metadata: None,
    });

    // Read SSE frames until we see the event (stream never closes on its own).
    let collected =
        collect_sse_frames_until(response.into_body(), |s| s.contains("o-sse-1"), 3000).await;
    assert!(
        collected.contains("o-sse-1"),
        "SSE body should contain the entity_id. Got: {collected}"
    );
    assert!(
        collected.contains("SubmitOrder"),
        "SSE body should contain the action. Got: {collected}"
    );
}

#[tokio::test]
async fn test_sse_events_lagged_receiver_continues() {
    let state = test_state_with_ioa();
    let event_tx = state.event_tx.clone();

    // The broadcast channel capacity is 256 (set in ServerState constructors).
    // Flood it before any subscriber — then subscribe and send one more event.
    for i in 0..300 {
        let _ = event_tx.send(EntityStateChange {
            seq: (i + 1) as u64,
            entity_type: "Order".into(),
            entity_id: format!("flood-{i}"),
            action: "Flood".into(),
            status: "Flooded".into(),
            tenant: "default".into(),
            agent_id: None,
            session_id: None,
            intent: None,
            observation_metadata: None,
        });
    }

    let app = authenticated_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/$events")
                .header("X-Temper-Principal-Kind", "admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Send a fresh event that should be delivered.
    let _ = event_tx.send(EntityStateChange {
        seq: 301,
        entity_type: "Order".into(),
        entity_id: "after-flood".into(),
        action: "Fresh".into(),
        status: "OK".into(),
        tenant: "default".into(),
        agent_id: None,
        session_id: None,
        intent: None,
        observation_metadata: None,
    });

    // Read frames — the stream should recover and deliver the fresh event.
    let collected =
        collect_sse_frames_until(response.into_body(), |s| s.contains("after-flood"), 3000).await;
    assert!(
        collected.contains("after-flood"),
        "SSE should recover after lag. Got: {collected}"
    );
}

#[test]
fn bridge_action_params_fallback_strips_control_keys() {
    let params = bridge_action_params(&serde_json::json!({
        "Name": "refs/heads/main",
        "bridge_principal": { "kind": "customer", "id": "user-1" },
        "bridge_response": { "status": 401 }
    }));
    assert_eq!(params["Name"], "refs/heads/main");
    assert!(params.get("bridge_principal").is_none());
    assert!(params.get("bridge_response").is_none());
}

#[test]
fn git_route_params_fall_back_to_smart_http_path_when_exact_endpoint_has_no_captures() {
    let params = git_route_params_for_http_dispatch(
        "git_refs_advertise",
        "/temperpaw/paw-agent.git/info/refs",
        std::collections::BTreeMap::new(),
    );

    assert_eq!(params.get("owner").map(String::as_str), Some("temperpaw"));
    assert_eq!(params.get("repo").map(String::as_str), Some("paw-agent"));
}

#[test]
fn git_route_params_keep_captured_values() {
    let mut captured = std::collections::BTreeMap::new();
    captured.insert("owner".to_string(), "captured".to_string());
    captured.insert("repo".to_string(), "repo".to_string());

    let params = git_route_params_for_http_dispatch(
        "git_receive_pack",
        "/temperpaw/paw-agent.git/git-receive-pack",
        captured,
    );

    assert_eq!(params.get("owner").map(String::as_str), Some("captured"));
    assert_eq!(params.get("repo").map(String::as_str), Some("repo"));
}

#[test]
fn route_param_inference_ignores_non_git_modules() {
    let params = git_route_params_for_http_dispatch(
        "some_json_endpoint",
        "/temperpaw/paw-agent.git/info/refs",
        std::collections::BTreeMap::new(),
    );

    assert!(params.is_empty());
}

#[test]
fn bridge_response_requires_structured_action_params() {
    // Same passthrough guard as bridge_principal: never honored
    // verbatim, never falls through to dispatch.
    let response = bridge_short_circuit_response(&serde_json::json!({
        "bridge_response": { "status": 200, "body": "client-controlled" }
    }))
    .expect("unstructured bridge_response still short-circuits");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[test]
fn malformed_bridge_response_fails_closed_as_bad_gateway() {
    // Presence of the key must never decay into a dispatch.
    let response = bridge_short_circuit_response(&serde_json::json!({
        "action_params": {},
        "bridge_response": { "status": "not-a-number" }
    }))
    .expect("malformed bridge_response still short-circuits");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let response = bridge_short_circuit_response(&serde_json::json!({
        "action_params": {},
        "bridge_response": { "status": 401, "headers": { "bad\nname": "x" } }
    }))
    .expect("invalid header still short-circuits");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[test]
fn bridge_short_circuit_response_returns_status_headers_body() {
    let callback = serde_json::json!({
        "action_params": {},
        "bridge_response": {
            "status": 401,
            "headers": { "WWW-Authenticate": "Basic realm=\"Genesis\"" },
            "body": "authentication required"
        }
    });

    let response = bridge_short_circuit_response(&callback).expect("short circuit should build");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("WWW-Authenticate")
            .and_then(|v| v.to_str().ok()),
        Some("Basic realm=\"Genesis\"")
    );
}

#[test]
fn bridge_short_circuit_response_absent_is_none() {
    assert!(bridge_short_circuit_response(&serde_json::json!({ "action_params": {} })).is_none());
    // Present-but-malformed never falls through to dispatch — it fails
    // closed (covered by malformed_bridge_response_fails_closed_as_bad_gateway).
    let response = bridge_short_circuit_response(&serde_json::json!({
        "action_params": {},
        "bridge_response": { "body": "x" }
    }))
    .expect("missing status still short-circuits");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[test]
fn credential_headers_are_not_forwarded_to_wasm_guests() {
    // ARN-208: a WASM guest reads everything in its invocation context's headers.
    // This asserts the invariant against the shared extraction point
    // (`guest_visible_headers`) rather than the classifier alone, so deleting the
    // filter inside that function fails here. It does NOT assert the dispatch call
    // site: re-inlining the filter_map in `dispatch_matched_route` would still pass.
    // A true wire-level assertion needs a guest fixture that echoes its invocation
    // context; that is deferred with the outbound-header work in ARN-346.
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("authorization", "Bearer caller-token"),
        ("proxy-authorization", "Basic proxy"),
        ("cookie", "session=abc"),
        ("x-api-key", "sk-live-123"),
        ("x-forwarded-authorization", "Bearer upstream"),
        ("x-forwarded-access-token", "at-456"),
        ("x-goog-iap-jwt-assertion", "iap-jwt"),
        ("cf-access-jwt-assertion", "cf-jwt"),
        ("x-amzn-oidc-accesstoken", "alb-access"),
        ("x-amzn-oidc-data", "alb-data"),
        ("x-amzn-oidc-identity", "alb-identity"),
        ("x-ms-token-aad-access-token", "aad-access"),
        ("x-ms-token-aad-id-token", "aad-id"),
        ("x-ms-token-aad-refresh-token", "aad-refresh"),
        ("x-auth-request-access-token", "oauth2p-access"),
        ("x-amz-security-token", "aws-token"),
        // Ordinary headers the guest legitimately needs.
        ("content-type", "application/json"),
        ("accept", "*/*"),
        ("user-agent", "curl/8"),
        ("x-request-id", "req-1"),
    ] {
        headers.insert(
            axum::http::HeaderName::from_static(name),
            axum::http::HeaderValue::from_static(value),
        );
    }

    // Forces the next author who grows the const to also plant a header/value here:
    // the name loop below iterates the const and would otherwise pass trivially for
    // an unplanted entry.
    assert_eq!(
        GUEST_FORBIDDEN_CREDENTIAL_HEADERS.len(),
        16,
        "a credential header was added or removed — plant it in this test too"
    );

    let visible = guest_visible_headers(&headers);
    let names: Vec<String> = visible
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();

    for forbidden in GUEST_FORBIDDEN_CREDENTIAL_HEADERS {
        assert!(
            !names.iter().any(|n| n == forbidden),
            "{forbidden} carries a caller credential and must not reach a guest; got {names:?}"
        );
    }
    // No credential VALUE survives either. This catches a renamed-but-same-value
    // leak, and — unlike the name loop, which iterates the const itself and so
    // shrinks with it — every planted secret is asserted independently, so removing
    // any single entry from GUEST_FORBIDDEN_CREDENTIAL_HEADERS fails here.
    let values: Vec<&str> = visible.iter().map(|(_, v)| v.as_str()).collect();
    for secret in [
        "Bearer caller-token",
        "Basic proxy",
        "session=abc",
        "sk-live-123",
        "Bearer upstream",
        "at-456",
        "iap-jwt",
        "cf-jwt",
        "alb-access",
        "alb-data",
        "alb-identity",
        "aad-access",
        "aad-id",
        "aad-refresh",
        "oauth2p-access",
        "aws-token",
    ] {
        assert!(
            !values.contains(&secret),
            "credential value {secret:?} leaked to the guest: {values:?}"
        );
    }
    // Ordinary request context still reaches the guest.
    for expected in ["content-type", "accept", "user-agent", "x-request-id"] {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} is not a credential and must still be forwarded; got {names:?}"
        );
    }
}

#[test]
fn credential_header_classifier_is_case_insensitive() {
    for name in [
        "Authorization",
        "COOKIE",
        "X-Api-Key",
        "Cf-Access-Jwt-Assertion",
    ] {
        assert!(
            is_credential_header(name),
            "{name} must be classified as a credential"
        );
    }
    assert!(!is_credential_header("x-temper-observe-session-id"));
    assert!(!is_credential_header("content-type"));
}
