use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

use crate::events::EntityStateChange;
use crate::request_context::AgentContext;
use crate::storage::StorageStack;

struct SameTenantPublicationHook;

#[async_trait::async_trait]
impl crate::state::BoundActionHook for SameTenantPublicationHook {
    fn requires_generation_handoff(&self, entity_type: &str, action: &str) -> bool {
        entity_type == "Order" && action.rsplit('.').next().unwrap_or(action) == "CancelOrder"
    }

    async fn after_bound_action(
        &self,
        ctx: crate::state::BoundActionHookContext<'_>,
    ) -> Result<Option<serde_json::Value>, String> {
        let publication = ctx
            .state
            .begin_spec_publication_after_drain(
                ctx.tenant,
                ctx.expected_generation.ok_or_else(|| {
                    "same-tenant publication hook did not receive a generation token".to_string()
                })?,
            )
            .await?;
        drop(publication);
        Ok(Some(serde_json::json!({"publicationWriter": "acquired"})))
    }
}

struct FailOnceSameTenantPublicationHook {
    attempts: std::sync::atomic::AtomicUsize,
}

struct CountingBoundActionHook {
    attempts: std::sync::atomic::AtomicUsize,
    fail_first: bool,
}

struct ReceiptFaultIdempotentHook {
    store: SimEventStore,
    invocations: std::sync::atomic::AtomicUsize,
    external_effects: std::sync::atomic::AtomicUsize,
    outputs: std::sync::Mutex<std::collections::BTreeMap<String, serde_json::Value>>,
}

struct InterveningGenerationHook;

#[async_trait::async_trait]
impl crate::state::BoundActionHook for InterveningGenerationHook {
    fn requires_generation_handoff(&self, entity_type: &str, action: &str) -> bool {
        entity_type == "Order" && action.rsplit('.').next().unwrap_or(action) == "CancelOrder"
    }

    async fn after_bound_action(
        &self,
        ctx: crate::state::BoundActionHookContext<'_>,
    ) -> Result<Option<serde_json::Value>, String> {
        let expected_generation = ctx.expected_generation.ok_or_else(|| {
            "intervening-generation hook did not receive a generation token".to_string()
        })?;
        let mut intervening = ctx.state.begin_spec_publication(ctx.tenant).await?;
        let intent = ServerState::spec_publication_intent(
            "router-test-intervening-generation",
            [("generation", b"new".as_slice())],
        );
        ctx.state
            .arm_spec_publication(&mut intervening, ctx.tenant, &intent)?;
        ctx.state
            .complete_spec_publication_retry(&mut intervening, ctx.tenant)?;
        drop(intervening);

        ctx.state
            .begin_spec_publication_after_drain(ctx.tenant, expected_generation)
            .await?;
        Ok(None)
    }
}

#[async_trait::async_trait]
impl crate::state::BoundActionHook for FailOnceSameTenantPublicationHook {
    fn requires_generation_handoff(&self, entity_type: &str, action: &str) -> bool {
        entity_type == "Order" && action.rsplit('.').next().unwrap_or(action) == "CancelOrder"
    }

    async fn after_bound_action(
        &self,
        ctx: crate::state::BoundActionHookContext<'_>,
    ) -> Result<Option<serde_json::Value>, String> {
        let mut publication = ctx
            .state
            .begin_spec_publication_after_drain(
                ctx.tenant,
                ctx.expected_generation.ok_or_else(|| {
                    "same-tenant publication hook did not receive a generation token".to_string()
                })?,
            )
            .await?;
        let intent = ServerState::spec_publication_intent(
            "router-test-post-action",
            [("generation", b"one".as_slice())],
        );
        ctx.state
            .arm_spec_publication(&mut publication, ctx.tenant, &intent)?;
        if self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            return Err("injected post-arm publication failure".to_string());
        }
        ctx.state
            .complete_spec_publication_retry(&mut publication, ctx.tenant)?;
        Ok(Some(serde_json::json!({"publicationRetry": "completed"})))
    }
}

#[async_trait::async_trait]
impl crate::state::BoundActionHook for CountingBoundActionHook {
    fn requires_generation_handoff(&self, entity_type: &str, action: &str) -> bool {
        entity_type == "Order" && action.rsplit('.').next().unwrap_or(action) == "CancelOrder"
    }

    async fn after_bound_action(
        &self,
        _ctx: crate::state::BoundActionHookContext<'_>,
    ) -> Result<Option<serde_json::Value>, String> {
        let attempt = self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if self.fail_first && attempt == 1 {
            return Err("injected first hook failure".to_string());
        }
        Ok(Some(serde_json::json!({"hookAttempt": attempt})))
    }
}

#[async_trait::async_trait]
impl crate::state::BoundActionHook for ReceiptFaultIdempotentHook {
    async fn after_bound_action(
        &self,
        ctx: crate::state::BoundActionHookContext<'_>,
    ) -> Result<Option<serde_json::Value>, String> {
        let invocation = self
            .invocations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let output = {
            let mut outputs = self.outputs.lock().expect("hook output lock");
            if let Some(output) = outputs.get(ctx.operation_id) {
                output.clone()
            } else {
                self.external_effects
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let output = serde_json::json!({"operationId": ctx.operation_id});
                outputs.insert(ctx.operation_id.to_string(), output.clone());
                output
            }
        };
        if invocation == 0 {
            self.store.restore_faults(temper_store_sim::SimFaultConfig {
                write_failure_prob: 1.0,
                concurrency_violation_prob: 0.0,
                read_truncation_prob: 0.0,
                snapshot_failure_prob: 0.0,
            });
        }
        Ok(Some(output))
    }
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
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap()
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
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap()
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
    let csdl_xml = r#"<?xml version="1.0" encoding="utf-8"?>
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
    let log_entry_ioa = r#"
[automaton]
name = "LogEntry"
states = ["Recorded"]
initial = "Recorded"

[[state]]
name = "Body"
type = "string"
initial = ""
"#;
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-data-only-fast-path");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("LogEntry".to_string(), log_entry_ioa.to_string());
    let mut state = ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap();
    state.set_storage_stack(StorageStack::from_turso(turso));
    state
}

#[tokio::test]
async fn test_service_document() {
    let app = build_router(test_state());
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
async fn test_metadata_endpoint() {
    let app = build_router(test_state());
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
async fn test_entity_set_listing() {
    let app = build_router(test_state());
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
    let app = build_router(test_state());
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
    let app = build_router(test_state_with_ioa());

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
async fn test_unknown_entity_set_returns_404() {
    let app = build_router(test_state());
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
    let app = build_router(test_state_with_ioa());
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
    let app = build_router(test_state_with_ioa());
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
async fn test_data_only_entity_create_fast_path_persists_projection_without_actor_spawn() {
    let state = test_state_with_data_only_ioa_and_turso().await;
    let app = build_router(state.clone());

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
    state.enable_commons_guardrails("default");
    state.enable_commons_guardrails("beta");
    let app = build_router(state.clone());

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
    let app = build_router(test_state_with_blob_ioa());
    let response = app
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "3")
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

    let response = build_router(state.clone())
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "3")
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
    let app = build_router(state.clone());

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
                .header("X-Repository-Id", "rp-alice")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from("abc"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(alice_first.status(), StatusCode::CREATED);

    let alice_exceeded = app
        .clone()
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "2")
                .header("X-Repository-Id", "rp-alice")
                .header("X-Temper-Principal-Id", "alice")
                .header("X-Temper-Principal-Kind", "customer")
                .body(Body::from("de"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(alice_exceeded.status(), StatusCode::PAYLOAD_TOO_LARGE);

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
    let app = build_router(state.clone());

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
async fn commons_storage_cap_serializes_concurrent_blob_writes_per_owner() {
    let state = test_state_with_storage_cap_ioa();
    state.enable_commons_guardrails("default");
    let app = build_router(state.clone());

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
    let second_app = app.clone();
    let (first, second) = tokio::join!(
        async move {
            first_app
                .oneshot(
                    Request::post("/tdata/Blobs/Temper.IngestRaw")
                        .header("Content-Type", "application/octet-stream")
                        .header("Content-Length", "4")
                        .header("X-Repository-Id", "rp-dana")
                        .header("X-Temper-Principal-Id", "dana")
                        .header("X-Temper-Principal-Kind", "customer")
                        .body(Body::from("abcd"))
                        .unwrap(),
                )
                .await
                .unwrap()
        },
        async move {
            second_app
                .oneshot(
                    Request::post("/tdata/Blobs/Temper.IngestRaw")
                        .header("Content-Type", "application/octet-stream")
                        .header("Content-Length", "4")
                        .header("X-Repository-Id", "rp-dana")
                        .header("X-Temper-Principal-Id", "dana")
                        .header("X-Temper-Principal-Kind", "customer")
                        .body(Body::from("wxyz"))
                        .unwrap(),
                )
                .await
                .unwrap()
        }
    );

    let mut statuses = vec![first.status(), second.status()];
    statuses.sort();
    assert_eq!(
        statuses,
        vec![StatusCode::CREATED, StatusCode::PAYLOAD_TOO_LARGE]
    );

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
    let app = build_router(state.clone());

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
    let app = build_router(state);

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
    let app = build_router(test_state_with_ioa());
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
async fn same_tenant_post_action_publication_hands_off_request_generation() {
    let mut state = test_state_with_ioa();
    state.bound_action_hook = Some(std::sync::Arc::new(SameTenantPublicationHook));
    let response = build_router(state)
        .oneshot(
            Request::post("/tdata/Orders('same-tenant')/Temper.Example.CancelOrder")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .header("Idempotency-Key", "same-tenant-publication")
                .body(Body::from(r#"{"Reason": "publish generation"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["postAction"]["publicationWriter"], "acquired");
}

#[tokio::test]
async fn publication_context_is_the_only_actor_path_inside_an_armed_generation() {
    let state = test_state_with_ioa();
    let tenant = TenantId::default();
    let intent = ServerState::spec_publication_intent(
        "router-test-internal-publication",
        [("generation", b"one".as_slice())],
    );
    let mut publication = state
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire publication writer");
    state
        .arm_spec_publication(&mut publication, &tenant, &intent)
        .expect("arm publication writer");

    assert!(
        state
            .get_or_spawn_tenant_actor(&tenant, "Order", "publication-owned")
            .is_none(),
        "unscoped actor resolution must remain fenced while publication is armed"
    );
    let publication_ctx = state
        .spec_publication_dispatch_context(&publication, &tenant, "router-test")
        .expect("derive publication-owned dispatch context");
    state
        .get_or_create_tenant_entity_in_generation(
            &tenant,
            "Order",
            "publication-owned",
            serde_json::json!({}),
            &publication_ctx,
        )
        .await
        .expect("publication-owned actor resolution must use the live table");

    state
        .complete_spec_publication(&mut publication, &tenant)
        .expect("complete first generation");
    drop(publication);

    let mut next_publication = state
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire next publication writer");
    state
        .arm_spec_publication(&mut next_publication, &tenant, &intent)
        .expect("arm next publication writer");
    state
        .get_tenant_entity_state_in_generation(
            &tenant,
            "Order",
            "publication-owned",
            &publication_ctx,
        )
        .await
        .expect_err("a context from the retired generation must not respawn an actor");

    let next_ctx = state
        .spec_publication_dispatch_context(&next_publication, &tenant, "router-test")
        .expect("derive next publication-owned context");
    state
        .get_tenant_entity_state_in_generation(&tenant, "Order", "publication-owned", &next_ctx)
        .await
        .expect("the current publication generation may resolve the actor");
    state
        .complete_spec_publication(&mut next_publication, &tenant)
        .expect("complete next generation");
}

#[tokio::test]
async fn detached_work_cannot_reenter_after_its_captured_generation_retires() {
    let state = test_state_with_ioa();
    let tenant = TenantId::default();
    let captured_generation = state.tenant_generation_version(&tenant);
    let intent = ServerState::spec_publication_intent(
        "router-test-detached-generation",
        [("generation", b"next".as_slice())],
    );
    let mut publication = state
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire publication writer");
    state
        .arm_spec_publication(&mut publication, &tenant, &intent)
        .expect("arm publication");
    assert!(
        state
            .try_begin_captured_tenant_generation(&tenant, captured_generation)
            .await
            .is_none(),
        "paused detached work must not enter while publication is armed"
    );
    state
        .complete_spec_publication(&mut publication, &tenant)
        .expect("complete replacement generation");
    drop(publication);
    assert!(
        state
            .try_begin_captured_tenant_generation(&tenant, captured_generation)
            .await
            .is_none(),
        "old-event work must not borrow the replacement runtime generation"
    );
    let current_generation = state.tenant_generation_version(&tenant);
    assert!(
        state
            .try_begin_captured_tenant_generation(&tenant, current_generation)
            .await
            .is_some(),
        "work captured from the current generation remains admissible"
    );
}

#[tokio::test]
async fn captured_generation_lease_holds_the_dispatch_boundary_against_publication() {
    let state = test_state_with_ioa();
    let tenant = TenantId::default();
    let captured_generation = state.tenant_generation_version(&tenant);
    let lease = state
        .try_begin_captured_tenant_generation(&tenant, captured_generation)
        .await
        .expect("captured work should re-enter the current generation");
    let dispatch_ctx =
        AgentContext::for_service("boundary-test").with_tenant_generation_lease(lease);

    let busy = match state.begin_spec_publication(&tenant).await {
        Ok(_) => panic!("publication crossed an active captured dispatch generation"),
        Err(error) => error,
    };
    assert!(busy.contains("runtime generation is busy"));

    drop(dispatch_ctx);
    let mut publication = state
        .begin_spec_publication(&tenant)
        .await
        .expect("publisher should acquire after the dispatch context drops");
    let intent = ServerState::spec_publication_intent(
        "boundary-held-generation",
        [("generation", b"next".as_slice())],
    );
    state
        .arm_spec_publication(&mut publication, &tenant, &intent)
        .expect("arm replacement generation");
    state
        .complete_spec_publication(&mut publication, &tenant)
        .expect("complete replacement generation");
}

#[tokio::test]
async fn immediate_generation_fork_drains_before_publication_cutover() {
    let state = test_state_with_ioa();
    let tenant = TenantId::default();
    let generation = state.tenant_generation_version(&tenant);
    let request = state
        .try_begin_captured_tenant_generation(&tenant, generation)
        .await
        .expect("capture request generation");
    let immediate = request
        .fork_immediate(&tenant)
        .expect("fork immediate obligation");
    request.release();

    let busy = match state.begin_spec_publication(&tenant).await {
        Ok(_) => panic!("publication must drain the independently forked obligation"),
        Err(error) => error,
    };
    assert!(busy.contains("runtime generation is busy"));

    drop(immediate);
    state
        .begin_spec_publication(&tenant)
        .await
        .expect("publication may begin after the immediate obligation completes");
}

#[tokio::test]
async fn released_request_lease_cannot_borrow_a_publication_gate() {
    let state = test_state_with_ioa();
    let tenant = TenantId::default();
    let generation = state.tenant_generation_version(&tenant);
    let lease = state
        .try_begin_captured_tenant_generation(&tenant, generation)
        .await
        .expect("capture request generation");
    let agent_ctx = AgentContext::for_service("released-request-test")
        .with_tenant_generation_lease(lease.clone());
    lease.release();

    let mut publication = state
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire publication writer after request release");
    let intent = ServerState::spec_publication_intent(
        "released-request-provenance",
        [("generation", b"pending".as_slice())],
    );
    state
        .arm_spec_publication(&mut publication, &tenant, &intent)
        .expect("arm pending generation");

    let error = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "released-request",
            "AddItem",
            serde_json::json!({}),
            &agent_ctx,
        )
        .await
        .expect_err("released request provenance must not dispatch inside a writer gate");
    assert!(
        error.contains("dispatch deferred"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn publication_owned_background_work_waits_for_the_pending_generation() {
    let state = test_state_with_ioa();
    let tenant = TenantId::default();
    let pending_generation = state
        .tenant_generation_version(&tenant)
        .checked_add(1)
        .expect("test generation should not overflow");
    let mut publication = state
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire publication writer");
    let intent = ServerState::spec_publication_intent(
        "queued-publication-work",
        [("generation", b"next".as_slice())],
    );
    state
        .arm_spec_publication(&mut publication, &tenant, &intent)
        .expect("arm pending generation");

    let queued_state = state.clone();
    let queued_tenant = tenant.clone();
    let (finished_tx, mut finished_rx) = tokio::sync::oneshot::channel();
    let queued = tokio::spawn(async move {
        let lease = queued_state
            .begin_captured_tenant_generation(&queued_tenant, pending_generation)
            .await;
        let _ = finished_tx.send(lease.is_some());
    });
    tokio::task::yield_now().await;
    assert!(
        matches!(
            finished_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "publication-owned effects must wait behind the writer boundary"
    );

    state
        .complete_spec_publication(&mut publication, &tenant)
        .expect("complete pending generation");
    drop(publication);
    assert!(
        finished_rx
            .await
            .expect("queued background task should report its admission"),
        "queued work should enter the exact newly published generation"
    );
    queued.await.expect("queued task should finish");
}

#[tokio::test]
async fn sticky_debt_retry_cannot_enter_while_an_exact_retry_writer_is_active() {
    let state = test_state_with_ioa();
    let tenant = TenantId::default();
    let intent = ServerState::spec_publication_intent(
        "sticky-debt-retry-race",
        [("generation", b"same".as_slice())],
    );
    let mut ambiguous = state
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire first publication writer");
    state
        .arm_spec_publication(&mut ambiguous, &tenant, &intent)
        .expect("arm first publication");
    drop(ambiguous);
    assert!(state.spec_publication_gated(&tenant));

    let first_retry_reader = state
        .try_begin_tenant_request(&tenant)
        .await
        .expect("idle sticky debt should admit one stable retry reader");
    drop(first_retry_reader);
    let mut active_retry = state
        .begin_spec_publication(&tenant)
        .await
        .expect("exact retry should acquire the writer after handoff");
    state
        .arm_spec_publication(&mut active_retry, &tenant, &intent)
        .expect("exact retry should inherit the same debt");

    assert!(
        state.try_begin_tenant_request(&tenant).await.is_none(),
        "a second retry must not read registry, Cedar, or actors during the active cutover"
    );
    state
        .complete_spec_publication_retry(&mut active_retry, &tenant)
        .expect("exact retry should discharge sticky debt");
}

#[tokio::test]
async fn same_tenant_post_action_rejects_an_intervening_generation() {
    let mut state = test_state_with_ioa();
    state.bound_action_hook = Some(std::sync::Arc::new(InterveningGenerationHook));
    let tenant = TenantId::default();
    let response = build_router(state.clone())
        .oneshot(
            Request::post("/tdata/Orders('generation-race')/Temper.Example.CancelOrder")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .header("Idempotency-Key", "generation-race-publication")
                .body(Body::from(r#"{"Reason": "publish generation"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(state.tenant_generation_version(&tenant), 1);
}

#[tokio::test]
async fn publication_capable_action_requires_idempotency_before_transition() {
    let mut state = test_state_with_ioa();
    state.bound_action_hook = Some(std::sync::Arc::new(SameTenantPublicationHook));
    let tenant = TenantId::default();
    let response = build_router(state.clone())
        .oneshot(
            Request::post("/tdata/Orders('keyless-publication')/Temper.Example.CancelOrder")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .body(Body::from(r#"{"Reason": "must not transition"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!state.entity_exists(&tenant, "Order", "keyless-publication"));
    assert!(!state.spec_publication_gated(&tenant));
}

#[tokio::test]
async fn unproved_actor_cache_entry_cannot_be_rebound_to_bound_action() {
    let state = test_state_with_ioa();
    let tenant = TenantId::default();
    let seeded = state
        .get_or_create_tenant_entity(&tenant, "Order", "unproved-cache", serde_json::json!({}))
        .await
        .unwrap();
    state
        .idempotency_cache
        .put("default:Order:unproved-cache", "shared-raw-key", seeded);

    let response = build_router(state)
        .oneshot(
            Request::post("/tdata/Orders('unproved-cache')/Temper.Example.CancelOrder")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .header("Idempotency-Key", "shared-raw-key")
                .body(Body::from(r#"{"Reason": "must conflict"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn concurrent_bound_actions_cannot_rebind_one_raw_idempotency_key() {
    let app = build_router(test_state_with_ioa());
    let first = app.clone().oneshot(
        Request::post("/tdata/Orders('concurrent-key')/Temper.Example.CancelOrder")
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("Idempotency-Key", "concurrent-raw-key")
            .body(Body::from(r#"{"Reason": "first"}"#))
            .unwrap(),
    );
    let second = app.oneshot(
        Request::post("/tdata/Orders('concurrent-key')/Temper.Example.CancelOrder")
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("Idempotency-Key", "concurrent-raw-key")
            .body(Body::from(r#"{"Reason": "second"}"#))
            .unwrap(),
    );
    let (first, second) = tokio::join!(first, second);
    let mut statuses = [first.unwrap().status(), second.unwrap().status()];
    statuses.sort();

    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
}

#[tokio::test]
async fn exact_bound_action_retry_reuses_the_completed_post_action_output() {
    let mut state = test_state_with_ioa();
    let hook = std::sync::Arc::new(CountingBoundActionHook {
        attempts: std::sync::atomic::AtomicUsize::new(0),
        fail_first: false,
    });
    state.bound_action_hook = Some(hook.clone());
    let app = build_router(state);
    let request = || {
        Request::post("/tdata/Orders('hook-once')/Temper.Example.CancelOrder")
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("Idempotency-Key", "hook-once-key")
            .body(Body::from(r#"{"Reason": "one hook"}"#))
            .unwrap()
    };

    let first = app.clone().oneshot(request()).await.unwrap();
    let retry = app.oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(retry.status(), StatusCode::OK);
    let first: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(first.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let retry: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(retry.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(retry["postAction"], first["postAction"]);
    assert_eq!(
        hook.attempts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a completed post-action hook must not run again on an exact retry"
    );
}

#[tokio::test]
async fn completed_bound_action_hook_survives_cache_loss_and_actor_respawn() {
    let mut state = test_state_with_ioa();
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(409), None));
    let hook = std::sync::Arc::new(CountingBoundActionHook {
        attempts: std::sync::atomic::AtomicUsize::new(0),
        fail_first: false,
    });
    state.bound_action_hook = Some(hook.clone());
    let app = build_router(state.clone());
    let request = || {
        Request::post("/tdata/Orders('hook-restart')/Temper.Example.CancelOrder")
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("Idempotency-Key", "hook-restart-key")
            .body(Body::from(r#"{"Reason": "durable hook"}"#))
            .unwrap()
    };

    let first = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(first.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();

    let actor_key = "default:Order:hook-restart";
    state.idempotency_cache.clear_actor_for_test(actor_key);
    state
        .last_accessed
        .write()
        .expect("last-accessed lock")
        .insert(
            actor_key.to_string(),
            temper_runtime::scheduler::sim_now() - chrono::Duration::seconds(600),
        );
    state.passivate_idle_actors().await;
    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(actor_key),
        "fixture must remove the live actor before retry"
    );

    let retry = app.oneshot(request()).await.unwrap();
    assert_eq!(retry.status(), StatusCode::OK);
    let retry: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(retry.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(retry["postAction"], first["postAction"]);
    assert_eq!(
        hook.attempts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a completed durable hook must not execute again after actor respawn"
    );
}

#[tokio::test]
async fn pending_hook_receipt_replays_one_content_idempotent_operation_after_respawn() {
    let store = SimEventStore::no_faults(419);
    let mut state = test_state_with_ioa();
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let hook = std::sync::Arc::new(ReceiptFaultIdempotentHook {
        store: store.clone(),
        invocations: std::sync::atomic::AtomicUsize::new(0),
        external_effects: std::sync::atomic::AtomicUsize::new(0),
        outputs: std::sync::Mutex::new(std::collections::BTreeMap::new()),
    });
    state.bound_action_hook = Some(hook.clone());
    let app = build_router(state.clone());
    let request = || {
        Request::post("/tdata/Orders('hook-receipt-fault')/Temper.Example.CancelOrder")
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("Idempotency-Key", "hook-receipt-fault-key")
            .body(Body::from(r#"{"Reason": "receipt failure"}"#))
            .unwrap()
    };

    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "the hook commits before its injected completion-receipt failure"
    );
    store.disable_faults();

    let actor_key = "default:Order:hook-receipt-fault";
    state.idempotency_cache.clear_actor_for_test(actor_key);
    state
        .last_accessed
        .write()
        .expect("last-accessed lock")
        .insert(
            actor_key.to_string(),
            temper_runtime::scheduler::sim_now() - chrono::Duration::seconds(600),
        );
    state.passivate_idle_actors().await;

    let recovered = app.clone().oneshot(request()).await.unwrap();
    let exact_retry = app.oneshot(request()).await.unwrap();
    assert_eq!(recovered.status(), StatusCode::OK);
    assert_eq!(exact_retry.status(), StatusCode::OK);
    let recovered: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(recovered.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let exact_retry: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(exact_retry.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(exact_retry["postAction"], recovered["postAction"]);
    assert_eq!(
        hook.invocations.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the pending durable intent is retried exactly once"
    );
    assert_eq!(
        hook.external_effects
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the stable operation ID deduplicates the external mutation"
    );
}

#[tokio::test]
async fn failed_bound_action_hook_retries_once_then_caches_its_success() {
    let mut state = test_state_with_ioa();
    let hook = std::sync::Arc::new(CountingBoundActionHook {
        attempts: std::sync::atomic::AtomicUsize::new(0),
        fail_first: true,
    });
    state.bound_action_hook = Some(hook.clone());
    let app = build_router(state);
    let request = || {
        Request::post("/tdata/Orders('hook-retry')/Temper.Example.CancelOrder")
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("Idempotency-Key", "hook-retry-key")
            .body(Body::from(r#"{"Reason": "retry hook"}"#))
            .unwrap()
    };

    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let recovered = app.clone().oneshot(request()).await.unwrap();
    let retry = app.oneshot(request()).await.unwrap();
    assert_eq!(recovered.status(), StatusCode::OK);
    assert_eq!(retry.status(), StatusCode::OK);
    let recovered: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(recovered.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let retry: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(retry.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(retry["postAction"], recovered["postAction"]);
    assert_eq!(
        hook.attempts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "a successful hook retry must become the cached terminal result"
    );
}

#[tokio::test]
async fn gated_idempotent_post_action_retry_completes_exact_publication() {
    let mut state = test_state_with_ioa();
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(401), None));
    state.bound_action_hook = Some(std::sync::Arc::new(FailOnceSameTenantPublicationHook {
        attempts: std::sync::atomic::AtomicUsize::new(0),
    }));
    let tenant = TenantId::default();
    let app = build_router(state.clone());
    let request = || {
        Request::post("/tdata/Orders('gated-retry')/Temper.Example.CancelOrder")
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("X-Temper-Principal-Id", "original-operator")
            .header("X-Session-Id", "publication-session-one")
            .header("Idempotency-Key", "gated-retry-1")
            .body(Body::from(r#"{"Reason": "retry publication"}"#))
            .unwrap()
    };

    let failed = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(state.spec_publication_gated(&tenant));
    state
        .idempotency_cache
        .clear_actor_for_test("default:Order:gated-retry");

    let changed_params = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders('gated-retry')/Temper.Example.CancelOrder")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .header("X-Temper-Principal-Id", "other-operator")
                .header("Idempotency-Key", "gated-retry-1")
                .body(Body::from(r#"{"Reason": "different target"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let changed_params_status = changed_params.status();
    let changed_params_body = axum::body::to_bytes(changed_params.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        changed_params_status,
        StatusCode::CONFLICT,
        "unexpected changed-params response: {}",
        String::from_utf8_lossy(&changed_params_body)
    );
    assert!(state.spec_publication_gated(&tenant));

    let changed_principal = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders('gated-retry')/Temper.Example.CancelOrder")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "customer")
                .header("X-Temper-Principal-Id", "different-principal")
                .header("Idempotency-Key", "gated-retry-1")
                .body(Body::from(r#"{"Reason": "retry publication"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(changed_principal.status(), StatusCode::CONFLICT);
    assert!(state.spec_publication_gated(&tenant));

    let changed_action = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders('gated-retry')/Temper.Example.InitiateReturn")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .header("Idempotency-Key", "gated-retry-1")
                .body(Body::from(r#"{"Reason": "retry publication"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(changed_action.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(state.spec_publication_gated(&tenant));

    let recovered = app
        .oneshot(
            Request::post("/tdata/Orders('gated-retry')/Temper.Example.CancelOrder")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .header("X-Temper-Principal-Id", "recovery-operator")
                .header("X-Session-Id", "publication-session-two")
                .header("Idempotency-Key", "gated-retry-1")
                .body(Body::from(r#"{"Reason": "retry publication"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(recovered.status(), StatusCode::OK);
    let body = axum::body::to_bytes(recovered.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["postAction"]["publicationRetry"], "completed");
    assert!(!state.spec_publication_gated(&tenant));
}

#[tokio::test]
async fn test_odata_version_header() {
    let app = build_router(test_state());
    let response = app
        .oneshot(Request::get("/tdata/Orders").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let odata_version = response.headers().get("OData-Version").unwrap();
    assert_eq!(odata_version, "4.0");
}

#[tokio::test]
async fn test_old_odata_path_returns_404() {
    let app = build_router(test_state());
    let response = app
        .oneshot(Request::get("/odata").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_post_body_used_for_entity_creation() {
    let app = build_router(test_state_with_ioa());

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
    let app = build_router(test_state_with_ioa());

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
    let app = build_router(test_state_with_ioa());

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
    let app = build_router(test_state_with_ioa());

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
    let app = build_router(test_state_with_ioa());
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
    let app = build_router(test_state_with_ioa());
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
    let app = build_router(test_state_with_order_and_payment_ioa());

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

    let response = build_router(state)
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
    let app = build_router(test_state_with_ioa());
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
    let app = build_router(test_state());
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
    let app = build_router(test_state());
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
    let app = build_router(test_state());
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
    let app = build_router(state);

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

    let app = build_router(state);
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
fn bridge_resolved_principal_builds_security_context_with_scopes() {
    let callback = serde_json::json!({
        "action_params": {},
        "bridge_principal": {
            "kind": "customer",
            "id": "user-rita",
            "scopes": ["repo:push", "force", " ", ""]
        }
    });

    let ctx = bridge_resolved_principal(&callback).expect("principal should resolve");

    assert_eq!(ctx.principal.id, "user-rita");
    assert!(matches!(
        ctx.principal.kind,
        temper_authz::PrincipalKind::Customer
    ));
    let scopes = ctx
        .principal
        .attributes
        .get("scopes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(scopes.contains(&serde_json::Value::String("repo:push".to_string())));
    assert!(scopes.contains(&serde_json::Value::String("force".to_string())));
    assert_eq!(scopes.len(), 2);
}

#[test]
fn bridge_resolved_principal_rejects_missing_or_empty_identity() {
    assert!(bridge_resolved_principal(&serde_json::json!({ "action_params": {} })).is_none());
    assert!(
        bridge_resolved_principal(&serde_json::json!({
            "action_params": {},
            "bridge_principal": { "kind": "customer", "id": "  " }
        }))
        .is_none()
    );
    assert!(
        bridge_resolved_principal(&serde_json::json!({
            "action_params": {},
            "bridge_principal": { "kind": "", "id": "user-1" }
        }))
        .is_none()
    );
}

#[test]
fn bridge_resolved_principal_requires_structured_action_params() {
    // A passthrough adapter (top-level params, no action_params key)
    // must never hand the caller an identity (ADR-0138).
    assert!(
        bridge_resolved_principal(&serde_json::json!({
            "bridge_principal": { "kind": "customer", "id": "user-1" }
        }))
        .is_none()
    );
}

#[test]
fn bridge_resolved_principal_cannot_smuggle_system_kind() {
    let ctx = bridge_resolved_principal(&serde_json::json!({
        "action_params": {},
        "bridge_principal": { "kind": "system", "id": "evil" }
    }))
    .expect("principal should resolve");
    assert!(matches!(
        ctx.principal.kind,
        temper_authz::PrincipalKind::Customer
    ));
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
fn http_endpoint_fallback_tenant_prefers_default_over_sort_order() {
    // Regression: tenants that sort before "default" (e.g. Directed
    // Evolution control tenants on production) must not capture
    // header-less protocol requests.
    let de = TenantId::new("de-control-agent-answers");
    let default = TenantId::new("default");
    let other = TenantId::new("acme");
    let ids = vec![&other, &de, &default];
    assert_eq!(http_endpoint_fallback_tenant(&ids), Some(&default));
}

#[test]
fn http_endpoint_fallback_tenant_skips_system_then_takes_first() {
    let system = TenantId::new("temper-system");
    let acme = TenantId::new("acme");
    assert_eq!(
        http_endpoint_fallback_tenant(&[&system, &acme]),
        Some(&acme)
    );
    // Only the system tenant registered: still resolves rather than 404.
    assert_eq!(http_endpoint_fallback_tenant(&[&system]), Some(&system));
    assert_eq!(http_endpoint_fallback_tenant(&[]), None);
}
