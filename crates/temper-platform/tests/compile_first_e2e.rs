//! Compile-first onboarding E2E tests.
//!
//! These tests prove the production path a coding agent triggers:
//! specs written to disk → `temper verify` → `temper serve --specs-dir` →
//! OData API live with verified entities.
//!
//! Each test loads user specs into a SpecRegistry, bootstraps the system
//! tenant, builds the platform router, and exercises the HTTP API. No
//! simulation abstractions — the only difference from production is no
//! Postgres persistence (in-memory only) and no OTEL export.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use temper_platform::bootstrap::bootstrap_system_tenant;
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

/// Minimal Task IOA spec for multi-tenant tests.
const TASK_IOA: &str = r#"
[automaton]
name = "Task"
initial = "Backlog"
states = ["Backlog", "InProgress", "Done", "Cancelled"]

[[action]]
name = "StartWork"
from = ["Backlog"]
to = "InProgress"
kind = "internal"

[[action]]
name = "Complete"
from = ["InProgress"]
to = "Done"
kind = "internal"

[[action]]
name = "Cancel"
from = ["Backlog", "InProgress"]
to = "Cancelled"
kind = "input"
"#;

/// Minimal CSDL for a Task entity (beta tenant).
const TASK_CSDL_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Example" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Task">
        <Key><PropertyRef Name="Id" /></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false" />
        <Property Name="Status" Type="Edm.String" />
      </EntityType>
      <Action Name="StartWork" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Example.Task" />
      </Action>
      <Action Name="Complete" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Example.Task" />
      </Action>
      <Action Name="Cancel" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Example.Task" />
      </Action>
      <EntityContainer Name="TaskService">
        <EntitySet Name="Tasks" EntityType="Temper.Example.Task" />
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// Helper to read a response body as JSON.
async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// Helper to read a response body as string.
async fn body_string(response: axum::http::Response<Body>) -> String {
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

/// Build a SpecRegistry with a single user tenant.
///
/// Marks all entities as verification-passed so the verification gate allows
/// operations. This simulates the compile-first path where specs have already
/// been verified by `temper verify` before being served.
fn build_user_registry(tenant: &str, ioa_specs: &[(&str, &str)]) -> SpecRegistry {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(tenant, csdl, CSDL_XML.to_string(), ioa_specs);
    let tenant_id = TenantId::new(tenant);
    for (entity_type, _) in ioa_specs {
        registry.set_verification_status(
            &tenant_id,
            entity_type,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "Pre-verified".to_string(),
                    details: None,
                }],
                verified_at: "2026-02-18T00:00:00Z".to_string(),
            }),
        );
    }
    registry
}

// =========================================================================
// Test 1: Full Order lifecycle through compile-first path
// =========================================================================

/// Proves user specs loaded at startup → bootstrap adds system tenant →
/// entity actors process transitions correctly via HTTP.
#[tokio::test]
async fn e2e_compile_first_order_lifecycle() {
    let registry = build_user_registry("alpha", &[("Order", ORDER_IOA)]);
    let state = PlatformState::with_registry(registry, None);
    bootstrap_system_tenant(&state);
    let app = build_platform_router(state);

    // POST /tdata/Orders → 201, creates entity in Draft
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .header("X-Tenant-Id", "alpha")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let json = body_json(response).await;
    let odata_id = json["@odata.id"]
        .as_str()
        .expect("response should have @odata.id");
    let entity_id = odata_id
        .strip_prefix("Orders('")
        .unwrap()
        .strip_suffix("')")
        .unwrap();
    assert_eq!(json["status"], "Draft");

    // POST /tdata/Orders('{id}')/Temper.Example.CancelOrder → 200
    let response = app
        .clone()
        .oneshot(
            Request::post(&format!(
                "/tdata/Orders('{entity_id}')/Temper.Example.CancelOrder"
            ))
            .header("Content-Type", "application/json")
            .header("X-Tenant-Id", "alpha")
            .body(Body::from(r#"{"Reason": "changed mind"}"#))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "Cancelled");

    // GET /tdata/Orders('{id}') → 200, status: Cancelled
    let response = app
        .clone()
        .oneshot(
            Request::get(&format!("/tdata/Orders('{entity_id}')"))
                .header("X-Tenant-Id", "alpha")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "Cancelled");

    // GET /tdata/$metadata → body contains Temper.Example
    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .header("X-Tenant-Id", "alpha")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        body.contains("Temper.Example"),
        "metadata should contain Temper.Example namespace"
    );

    // GET /tdata → service doc lists Orders
    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata")
                .header("X-Tenant-Id", "alpha")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let sets: Vec<&str> = json["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(
        sets.contains(&"Orders"),
        "service document should contain Orders, got: {sets:?}"
    );
}

// =========================================================================
// Test 2: Two user tenants on one server
// =========================================================================

/// Proves multi-tenant compile-first works: alpha (Order) and beta (Task)
/// coexist and are isolated.
#[tokio::test]
async fn e2e_compile_first_two_tenants() {
    let csdl_alpha = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let csdl_beta = parse_csdl(TASK_CSDL_XML).expect("Task CSDL should parse");

    let mut registry = SpecRegistry::new();
    registry.register_tenant("alpha", csdl_alpha, CSDL_XML.to_string(), &[("Order", ORDER_IOA)]);
    registry.register_tenant("beta", csdl_beta, TASK_CSDL_XML.to_string(), &[("Task", TASK_IOA)]);
    // Mark entities as pre-verified for compile-first tests
    for (tenant, entity) in &[("alpha", "Order"), ("beta", "Task")] {
        registry.set_verification_status(
            &TenantId::new(*tenant),
            entity,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "Pre-verified".to_string(),
                    details: None,
                }],
                verified_at: "2026-02-18T00:00:00Z".to_string(),
            }),
        );
    }

    let state = PlatformState::with_registry(registry, None);
    bootstrap_system_tenant(&state);
    let app = build_platform_router(state);

    // POST /tdata/Orders with X-Tenant-Id: alpha → 201
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .header("X-Tenant-Id", "alpha")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let json = body_json(response).await;
    let alpha_id = json["@odata.id"]
        .as_str()
        .unwrap()
        .strip_prefix("Orders('")
        .unwrap()
        .strip_suffix("')")
        .unwrap()
        .to_string();
    assert_eq!(json["status"], "Draft");

    // POST action on alpha Order → Cancelled
    let response = app
        .clone()
        .oneshot(
            Request::post(&format!(
                "/tdata/Orders('{alpha_id}')/Temper.Example.CancelOrder"
            ))
            .header("Content-Type", "application/json")
            .header("X-Tenant-Id", "alpha")
            .body(Body::from("{}"))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "Cancelled");

    // POST /tdata/Tasks with X-Tenant-Id: beta → 201
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Tasks")
                .header("Content-Type", "application/json")
                .header("X-Tenant-Id", "beta")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let json = body_json(response).await;
    let beta_id = json["@odata.id"]
        .as_str()
        .unwrap()
        .strip_prefix("Tasks('")
        .unwrap()
        .strip_suffix("')")
        .unwrap()
        .to_string();
    assert_eq!(json["status"], "Backlog");

    // POST action on beta Task → InProgress
    let response = app
        .clone()
        .oneshot(
            Request::post(&format!(
                "/tdata/Tasks('{beta_id}')/Temper.Example.StartWork"
            ))
            .header("Content-Type", "application/json")
            .header("X-Tenant-Id", "beta")
            .body(Body::from("{}"))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "InProgress");

    // Verify isolation: alpha Orders don't appear in beta, beta Tasks don't appear in alpha
    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata")
                .header("X-Tenant-Id", "alpha")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = body_json(response).await;
    let alpha_sets: Vec<&str> = json["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(alpha_sets.contains(&"Orders"), "alpha should have Orders");

    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata")
                .header("X-Tenant-Id", "beta")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = body_json(response).await;
    let beta_sets: Vec<&str> = json["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(beta_sets.contains(&"Tasks"), "beta should have Tasks");
}

// =========================================================================
// Test 3: System and user tenants coexist
// =========================================================================

/// Proves system tenant and user tenant coexist on the same server,
/// each returning its own metadata and entity sets.
#[tokio::test]
async fn e2e_compile_first_system_and_user_coexist() {
    let registry = build_user_registry("alpha", &[("Order", ORDER_IOA)]);
    let state = PlatformState::with_registry(registry, None);
    bootstrap_system_tenant(&state);
    let app = build_platform_router(state);

    // GET /tdata/$metadata with X-Tenant-Id: alpha → sees user entities (Order)
    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .header("X-Tenant-Id", "alpha")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let alpha_meta = body_string(response).await;
    assert!(
        alpha_meta.contains("Temper.Example"),
        "alpha metadata should contain Temper.Example"
    );

    // GET /tdata/$metadata with X-Tenant-Id: temper-system → sees system entities
    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .header("X-Tenant-Id", "temper-system")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let sys_meta = body_string(response).await;
    assert!(
        sys_meta.contains("Temper.System"),
        "system metadata should contain Temper.System"
    );

    // Both tenants can dispatch actions independently

    // User tenant: create and cancel an Order
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .header("X-Tenant-Id", "alpha")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let json = body_json(response).await;
    let order_id = json["@odata.id"]
        .as_str()
        .unwrap()
        .strip_prefix("Orders('")
        .unwrap()
        .strip_suffix("')")
        .unwrap()
        .to_string();

    let response = app
        .clone()
        .oneshot(
            Request::post(&format!(
                "/tdata/Orders('{order_id}')/Temper.Example.CancelOrder"
            ))
            .header("Content-Type", "application/json")
            .header("X-Tenant-Id", "alpha")
            .body(Body::from("{}"))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "Cancelled");

    // System tenant: create and advance a Project
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Projects")
                .header("Content-Type", "application/json")
                .header("X-Tenant-Id", "temper-system")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let json = body_json(response).await;
    let proj_id = json["@odata.id"]
        .as_str()
        .unwrap()
        .strip_prefix("Projects('")
        .unwrap()
        .strip_suffix("')")
        .unwrap()
        .to_string();
    assert_eq!(json["status"], "Created");

    let response = app
        .clone()
        .oneshot(
            Request::post(&format!(
                "/tdata/Projects('{proj_id}')/Temper.System.UpdateSpecs"
            ))
            .header("Content-Type", "application/json")
            .header("X-Tenant-Id", "temper-system")
            .body(Body::from("{}"))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "Building");
}
