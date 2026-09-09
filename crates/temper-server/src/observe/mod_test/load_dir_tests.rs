use super::*;

// -- Load-dir endpoint tests --

#[tokio::test]
async fn test_load_dir_registers_specs() {
    let system = ActorSystem::new("test-load-dir");
    let registry = SpecRegistry::new();
    let state = ServerState::from_registry(system, registry);

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state.clone());

    // Use the test-fixtures/specs directory which has valid specs
    let specs_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-fixtures/specs");

    let body = serde_json::json!({
        "tenant": "test-tenant",
        "specs_dir": specs_dir.to_str().unwrap(),
    });

    let response = app
        .oneshot(with_tenant_security_context(
            Request::post("/api/specs/load-dir")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
            "test-tenant",
            temper_authz::SecurityContext::system(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    // Response is NDJSON — parse each line
    let body = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    let lines: Vec<serde_json::Value> = body_str
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // First line: specs_loaded
    assert_eq!(lines[0]["type"], "specs_loaded");
    assert_eq!(lines[0]["tenant"], "test-tenant");
    let entities = lines[0]["entities"].as_array().unwrap();
    assert!(
        !entities.is_empty(),
        "should have loaded at least one entity"
    );

    // Last line: summary
    let summary = lines.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["tenant"], "test-tenant");

    // Verify specs are in the registry
    let registry = state.registry.read().unwrap();
    let tenant_id: temper_runtime::tenant::TenantId = "test-tenant".into();
    let entity_types = registry.entity_types(&tenant_id);
    assert!(
        !entity_types.is_empty(),
        "registry should have entity types for test-tenant"
    );
}

#[tokio::test]
async fn test_load_dir_missing_dir_returns_error() {
    let system = ActorSystem::new("test-load-dir-missing");
    let registry = SpecRegistry::new();
    let state = ServerState::from_registry(system, registry);

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state);

    let body = serde_json::json!({
        "tenant": "test-tenant",
        "specs_dir": "/nonexistent/path/to/specs",
    });

    let response = app
        .oneshot(with_tenant_security_context(
            Request::post("/api/specs/load-dir")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
            "test-tenant",
            temper_authz::SecurityContext::system(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_load_dir_rejects_missing_and_cross_tenant_authority_before_fs_probe() {
    let state = ServerState::from_registry(
        ActorSystem::new("test-load-dir-auth-boundary"),
        SpecRegistry::new(),
    );
    let app = Router::new()
        .nest("/api", crate::api::build_api_router())
        .with_state(state);
    let body = serde_json::json!({
        "tenant": "victim",
        "specs_dir": "/a/path/whose/existence/must/not/be-probed",
    });
    let request = || {
        Request::post("/api/specs/load-dir")
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap()
    };

    let missing_context = app
        .clone()
        .oneshot(request())
        .await
        .expect("request should run");
    assert_eq!(missing_context.status(), StatusCode::UNAUTHORIZED);

    let same_tenant_denied = app
        .clone()
        .oneshot(with_tenant_security_context(
            request(),
            "victim",
            temper_authz::SecurityContext::from_resolved_identity(
                "agent-1",
                "swe",
                Some("session-1"),
            ),
        ))
        .await
        .expect("request should run");
    assert_eq!(same_tenant_denied.status(), StatusCode::FORBIDDEN);

    let wrong_tenant = app
        .oneshot(with_tenant_security_context(
            request(),
            "attacker",
            temper_authz::SecurityContext::from_resolved_identity(
                "agent-1",
                "swe",
                Some("session-1"),
            ),
        ))
        .await
        .expect("request should run");
    assert_eq!(wrong_tenant.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_load_dir_agent_needs_exact_directory_cedar_authority() {
    let state = ServerState::from_registry(
        ActorSystem::new("test-load-dir-exact-authz"),
        SpecRegistry::new(),
    );
    let app = Router::new()
        .nest("/api", crate::api::build_api_router())
        .with_state(state.clone());
    let specs_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-fixtures/specs");
    let canonical = std::fs::canonicalize(&specs_dir).expect("fixture path should canonicalize");
    let body = serde_json::json!({
        "tenant": "test-tenant",
        "specs_dir": canonical.to_str().unwrap(),
    });
    let request = || {
        with_tenant_security_context(
            Request::post("/api/specs/load-dir")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
            "test-tenant",
            temper_authz::SecurityContext::from_resolved_identity(
                "agent-1",
                "swe",
                Some("session-1"),
            ),
        )
    };

    let default_denied = app
        .clone()
        .oneshot(request())
        .await
        .expect("request should run");
    assert_eq!(default_denied.status(), StatusCode::FORBIDDEN);

    let resource_id = serde_json::to_string(canonical.to_str().unwrap()).unwrap();
    state
        .authz
        .reload_tenant_policies(
            "test-tenant",
            &format!(
                r#"
permit(
  principal == Agent::"agent-1",
  action == Action::"load_specs_from_directory",
  resource == SpecDirectory::{resource_id}
);
"#,
            ),
        )
        .expect("exact directory policy should parse");
    let allowed = app.oneshot(request()).await.expect("request should run");
    assert_eq!(allowed.status(), StatusCode::OK);
}

#[cfg(unix)]
#[tokio::test]
async fn test_load_dir_rejects_symlinked_directory_and_spec_file() {
    use std::os::unix::fs::symlink;

    let state = ServerState::from_registry(
        ActorSystem::new("test-load-dir-symlink"),
        SpecRegistry::new(),
    );
    let app = Router::new()
        .nest("/api", crate::api::build_api_router())
        .with_state(state);
    let fixture_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-fixtures/specs");
    let staging = tempfile::tempdir().expect("temp directory should build");
    let directory_link = staging.path().join("linked-specs");
    symlink(&fixture_dir, &directory_link).expect("directory symlink should build");
    let request_for = |specs_dir: &std::path::Path| {
        with_tenant_security_context(
            Request::post("/api/specs/load-dir")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "tenant": "test-tenant",
                        "specs_dir": specs_dir.to_str().unwrap(),
                    })
                    .to_string(),
                ))
                .unwrap(),
            "test-tenant",
            temper_authz::SecurityContext::system(),
        )
    };

    let linked_directory = app
        .clone()
        .oneshot(request_for(&directory_link))
        .await
        .expect("request should run");
    assert_eq!(linked_directory.status(), StatusCode::BAD_REQUEST);

    let file_link_root = staging.path().join("file-link-root");
    std::fs::create_dir(&file_link_root).expect("spec root should build");
    symlink(
        fixture_dir.join("model.csdl.xml"),
        file_link_root.join("model.csdl.xml"),
    )
    .expect("model symlink should build");
    std::fs::copy(
        fixture_dir.join("order.ioa.toml"),
        file_link_root.join("order.ioa.toml"),
    )
    .expect("IOA fixture should copy");
    let linked_file = app
        .oneshot(request_for(&file_link_root))
        .await
        .expect("request should run");
    assert_eq!(linked_file.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_load_dir_lint_error_aborts_registration() {
    let system = ActorSystem::new("test-load-dir-lint-error");
    let registry = SpecRegistry::new();
    let state = ServerState::from_registry(system, registry);

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state.clone());

    let temp_specs =
        std::env::temp_dir().join(format!("temper-load-dir-lint-{}", uuid::Uuid::new_v4())); // determinism-ok: test-only temp dir
    std::fs::create_dir_all(&temp_specs).expect("create temp specs dir"); // determinism-ok: test-only
    std::fs::write(
        // determinism-ok: test-only
        temp_specs.join("model.csdl.xml"),
        include_str!("../../../../../test-fixtures/specs/model.csdl.xml"),
    )
    .expect("write csdl");
    std::fs::write(
        // determinism-ok: test-only
        temp_specs.join("order.ioa.toml"),
        r#"
[automaton]
name = "Order"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
effect = "set phantom true"
"#,
    )
    .expect("write ioa");

    let body = serde_json::json!({
        "tenant": "lint-tenant",
        "specs_dir": temp_specs.to_str().unwrap(),
    });

    let response = app
        .oneshot(with_tenant_security_context(
            Request::post("/api/specs/load-dir")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
            "lint-tenant",
            temper_authz::SecurityContext::system(),
        ))
        .await
        .unwrap();

    let _ = std::fs::remove_dir_all(&temp_specs); // determinism-ok: test-only

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    let lines: Vec<serde_json::Value> = body_str
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(lines[0]["type"], "specs_loaded");
    assert!(lines.iter().any(|l| l["type"] == "lint_error"));
    assert!(!lines.iter().any(|l| l["type"] == "verification_started"));

    let registry = state.registry.read().unwrap();
    let tenant_id: temper_runtime::tenant::TenantId = "lint-tenant".into();
    assert!(
        registry.get_tenant(&tenant_id).is_none(),
        "tenant should not be registered when lint errors exist"
    );
}

#[tokio::test]
async fn test_load_dir_emits_design_time_events() {
    let db_url = format!(
        "file:/tmp/temper-design-time-test-{}.db",
        std::process::id(),
    );
    let turso = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let system = ActorSystem::new("test-load-dir-events");
    let registry = SpecRegistry::new();
    let mut state = ServerState::from_registry(system, registry);
    state.set_storage_stack(StorageStack::from_turso(turso));

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state.clone());

    let specs_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-fixtures/specs");

    let body = serde_json::json!({
        "tenant": "event-tenant",
        "specs_dir": specs_dir.to_str().unwrap(),
    });

    let response = app
        .oneshot(with_tenant_security_context(
            Request::post("/api/specs/load-dir")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
            "event-tenant",
            temper_authz::SecurityContext::system(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Consume entire body to wait for verification to complete
    let _ = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();

    // Check that design-time events were persisted to Turso.
    let turso = state.platform_turso_store().expect("turso configured");
    let events = turso
        .list_design_time_events(None, 1000)
        .await
        .expect("query design-time events from Turso");
    assert!(!events.is_empty(), "design-time events should be in Turso");

    // Should have spec_loaded, verify_started, and verify_done events
    let loaded_events: Vec<_> = events.iter().filter(|e| e.kind == "spec_loaded").collect();
    assert!(!loaded_events.is_empty(), "should have spec_loaded events");

    let started_events: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "verify_started")
        .collect();
    assert!(
        !started_events.is_empty(),
        "should have verify_started events"
    );

    let done_events: Vec<_> = events.iter().filter(|e| e.kind == "verify_done").collect();
    assert!(!done_events.is_empty(), "should have verify_done events");
}
