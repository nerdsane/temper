//! Platform E2E tests — shared registry proof.
//!
//! These tests prove that the shared `Arc<RwLock<SpecRegistry>>` fix works:
//! bootstrap writes and deploy writes are visible to HTTP dispatch. Every test
//! exercises the **production code path**: real `EntityActor` instances spawned
//! in a real `ActorSystem`, real `TransitionTable` evaluation, real tenant
//! dispatch through `ServerState`.
//!
//! No simulation abstractions — the only difference from production is no
//! Postgres persistence (in-memory only) and no OTEL export.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::tenant::TenantId;
use tower::ServiceExt;

use temper_platform::bootstrap::{SYSTEM_TENANT, bootstrap_system_tenant};
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;

// =========================================================================
// Dispatch-level tests — prove shared registry
// =========================================================================

/// Proves the core fix: bootstrap writes to `PlatformState.registry` are
/// visible to `ServerState.dispatch_tenant_action()`.
#[tokio::test]
async fn e2e_bootstrap_visible_to_dispatch() {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state);

    let tenant = TenantId::new(SYSTEM_TENANT);

    // Dispatch UpdateSpecs on a Project entity — this goes through
    // ServerState.get_or_spawn_tenant_actor() which reads the registry.
    let response = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Project",
            "p1",
            "UpdateSpecs",
            serde_json::json!({}),
        )
        .await
        .expect("dispatch should find temper-system tenant in registry");

    assert!(
        response.success,
        "UpdateSpecs should succeed: {:?}",
        response.error
    );
    assert_eq!(response.state.status, "Building");
}

/// Full Project lifecycle through dispatch: Created → Building → Verified → Archived.
#[tokio::test]
async fn e2e_project_lifecycle_via_dispatch() {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state);

    let tenant = TenantId::new(SYSTEM_TENANT);

    // Created → Building
    let r = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Project",
            "p2",
            "UpdateSpecs",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert!(r.success, "UpdateSpecs: {:?}", r.error);
    assert_eq!(r.state.status, "Building");

    // Building → Verified
    let r = state
        .server
        .dispatch_tenant_action(&tenant, "Project", "p2", "Verify", serde_json::json!({}))
        .await
        .unwrap();
    assert!(r.success, "Verify: {:?}", r.error);
    assert_eq!(r.state.status, "Verified");

    // Verified → Archived
    let r = state
        .server
        .dispatch_tenant_action(&tenant, "Project", "p2", "Archive", serde_json::json!({}))
        .await
        .unwrap();
    assert!(r.success, "Archive: {:?}", r.error);
    assert_eq!(r.state.status, "Archived");

    // Verify 3 events accumulated
    let r = state
        .server
        .get_tenant_entity_state(&tenant, "Project", "p2")
        .await
        .unwrap();
    assert_eq!(r.state.events.len(), 3);
}

/// Full Tenant lifecycle through dispatch: Pending → Active → Suspended → Active → Archived.
#[tokio::test]
async fn e2e_tenant_lifecycle_via_dispatch() {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state);

    let tenant = TenantId::new(SYSTEM_TENANT);

    // Pending → Active
    let r = state
        .server
        .dispatch_tenant_action(&tenant, "Tenant", "t1", "Deploy", serde_json::json!({}))
        .await
        .unwrap();
    assert!(r.success, "Deploy: {:?}", r.error);
    assert_eq!(r.state.status, "Active");

    // Active → Suspended
    let r = state
        .server
        .dispatch_tenant_action(&tenant, "Tenant", "t1", "Suspend", serde_json::json!({}))
        .await
        .unwrap();
    assert!(r.success, "Suspend: {:?}", r.error);
    assert_eq!(r.state.status, "Suspended");

    // Suspended → Active
    let r = state
        .server
        .dispatch_tenant_action(&tenant, "Tenant", "t1", "Reactivate", serde_json::json!({}))
        .await
        .unwrap();
    assert!(r.success, "Reactivate: {:?}", r.error);
    assert_eq!(r.state.status, "Active");

    // Active → Archived
    let r = state
        .server
        .dispatch_tenant_action(&tenant, "Tenant", "t1", "Archive", serde_json::json!({}))
        .await
        .unwrap();
    assert!(r.success, "Archive: {:?}", r.error);
    assert_eq!(r.state.status, "Archived");
}

/// Complete platform workflow — all 5 system entity types through dispatch.
#[tokio::test]
async fn e2e_full_platform_scenario() {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state);

    let tenant = TenantId::new(SYSTEM_TENANT);

    // 1. Project: UpdateSpecs → Building, Verify → Verified
    let r = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Project",
            "proj-1",
            "UpdateSpecs",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert!(r.success);
    let r = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Project",
            "proj-1",
            "Verify",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Verified");

    // 2. Collaborator: Accept → Active, ChangeRole → Active (non-transitioning)
    let r = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Collaborator",
            "col-1",
            "Accept",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Active");
    let r = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Collaborator",
            "col-1",
            "ChangeRole",
            serde_json::json!({"role": "editor"}),
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Active");

    // 3. Tenant: Deploy → Active
    let r = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Tenant",
            "tenant-1",
            "Deploy",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Active");

    // 4. Version: MarkDeployed → Deployed
    let r = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Version",
            "v-1",
            "MarkDeployed",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Deployed");

    // 5. CatalogEntry: Publish → Published
    let r = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "CatalogEntry",
            "cat-1",
            "Publish",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert!(r.success);
    assert_eq!(r.state.status, "Published");

    // Verify final states via get_tenant_entity_state
    let proj = state
        .server
        .get_tenant_entity_state(&tenant, "Project", "proj-1")
        .await
        .unwrap();
    assert_eq!(proj.state.status, "Verified");
    assert_eq!(proj.state.events.len(), 2);

    let col = state
        .server
        .get_tenant_entity_state(&tenant, "Collaborator", "col-1")
        .await
        .unwrap();
    assert_eq!(col.state.status, "Active");
    assert_eq!(col.state.events.len(), 2);

    let ten = state
        .server
        .get_tenant_entity_state(&tenant, "Tenant", "tenant-1")
        .await
        .unwrap();
    assert_eq!(ten.state.status, "Active");
    assert_eq!(ten.state.events.len(), 1);

    let ver = state
        .server
        .get_tenant_entity_state(&tenant, "Version", "v-1")
        .await
        .unwrap();
    assert_eq!(ver.state.status, "Deployed");
    assert_eq!(ver.state.events.len(), 1);

    let cat = state
        .server
        .get_tenant_entity_state(&tenant, "CatalogEntry", "cat-1")
        .await
        .unwrap();
    assert_eq!(cat.state.status, "Published");
    assert_eq!(cat.state.events.len(), 1);
}

// =========================================================================
// HTTP-level tests — same production code through axum
// =========================================================================

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

/// Full Project lifecycle through HTTP: POST create → POST UpdateSpecs → POST Verify → GET state.
#[tokio::test]
async fn e2e_http_project_lifecycle() {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state);
    let app = build_platform_router(state);

    // POST /tdata/Projects → 201, creates a new Project entity
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Projects")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"name": "test-project"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let json = body_json(response).await;
    let entity_id = json["@odata.id"]
        .as_str()
        .expect("response should have @odata.id");
    // Extract the key from "Projects('uuid')"
    let entity_id = entity_id
        .strip_prefix("Projects('")
        .unwrap()
        .strip_suffix("')")
        .unwrap();
    assert_eq!(json["status"], "Created");

    // POST /tdata/Projects('{id}')/Temper.System.UpdateSpecs → 200
    let response = app
        .clone()
        .oneshot(
            Request::post(format!(
                "/tdata/Projects('{entity_id}')/Temper.System.UpdateSpecs"
            ))
            .header("Content-Type", "application/json")
            .body(Body::from("{}"))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "Building");

    // POST /tdata/Projects('{id}')/Temper.System.Verify → 200
    let response = app
        .clone()
        .oneshot(
            Request::post(format!(
                "/tdata/Projects('{entity_id}')/Temper.System.Verify"
            ))
            .header("Content-Type", "application/json")
            .body(Body::from("{}"))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "Verified");

    // GET /tdata/$metadata → 200, contains "Temper.System"
    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        body.contains("Temper.System"),
        "metadata should contain Temper.System namespace"
    );

    // GET /tdata/Projects('{id}') → 200, status: Verified
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/tdata/Projects('{entity_id}')"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "Verified");
}

/// Metadata and service document show all system entity types after bootstrap.
#[tokio::test]
async fn e2e_http_metadata_shows_system_entities() {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state);
    let app = build_platform_router(state);

    // GET /tdata/$metadata → body contains all 5 entity types
    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata/$metadata")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    for entity_type in &[
        "Project",
        "Tenant",
        "CatalogEntry",
        "Collaborator",
        "Version",
    ] {
        assert!(
            body.contains(entity_type),
            "metadata should contain {entity_type}"
        );
    }

    // GET /tdata → service document lists all 5 entity sets
    let response = app
        .clone()
        .oneshot(Request::get("/tdata").body(Body::empty()).unwrap())
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
    for entity_set in &[
        "Projects",
        "Tenants",
        "CatalogEntries",
        "Collaborators",
        "Versions",
    ] {
        assert!(
            sets.contains(entity_set),
            "service document should contain {entity_set}, got: {sets:?}"
        );
    }
}
