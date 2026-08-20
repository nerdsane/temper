use super::*;
use crate::request_context::AgentContext;

const SCOPED_CONTINUITY_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Ready"]
initial = "Draft"

[[action]]
name = "Configure"
kind = "input"
from = ["Draft"]
to = "Ready"

[[action]]
name = "Simulate"
kind = "input"
from = ["Ready"]
to = "Ready"
"#;

#[tokio::test]
async fn task_scoped_read_requires_a_durable_entity_pin() {
    let digest = format!("sha256:{}", "a".repeat(64));
    let response = build_router(test_state_with_active_task_schema_and_ioa(
        SCOPED_CONTINUITY_IOA,
    ))
    .oneshot(
        Request::get("/tdata/Orders('missing-pin')")
            .header("X-Temper-Principal-Kind", "admin")
            .header("x-temper-schema-scope-kind", "task")
            .header("x-temper-schema-scope-id", "task-router")
            .header("x-temper-schema-bundle-digest", digest)
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "SchemaPinMismatch");
}

#[tokio::test]
async fn task_scoped_action_continuity_survives_restart_with_exact_digest() {
    let (state, store) =
        test_state_with_durable_active_task_schema_and_ioa(SCOPED_CONTINUITY_IOA).await;
    let digest = format!("sha256:{}", "a".repeat(64));
    let request = |action: Option<&str>| {
        let path = action.map_or_else(
            || "/tdata/Orders".to_string(),
            |action| format!("/tdata/Orders('restart-continuity')/Temper.ScopedExample.{action}"),
        );
        Request::post(path)
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("x-temper-schema-scope-kind", "task")
            .header("x-temper-schema-scope-id", "task-router")
            .header("x-temper-schema-bundle-digest", digest.as_str())
            .body(Body::from(if action.is_none() {
                r#"{"Id":"restart-continuity"}"#
            } else {
                "{}"
            }))
            .unwrap()
    };
    let app = build_router(state.clone());
    assert_eq!(
        app.clone().oneshot(request(None)).await.unwrap().status(),
        StatusCode::CREATED
    );
    assert_eq!(
        app.clone()
            .oneshot(request(Some("Configure")))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.oneshot(request(Some("Simulate")))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    drop(state);

    let mut restarted = test_state_with_ioa();
    restarted.set_storage_stack(StorageStack::from_sim(store, None));
    assert_eq!(
        build_router(restarted)
            .oneshot(request(Some("Simulate")))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn task_scoped_action_continuity_survives_turso_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!("file:{}", directory.path().join("pin-routing.db").display());
    let store = TursoEventStore::new(&database_url, None)
        .await
        .expect("create Turso store");
    persist_active_task_schema(&store, SCOPED_CONTINUITY_IOA).await;
    let mut state = test_state_with_active_task_schema_and_ioa(SCOPED_CONTINUITY_IOA);
    state.set_storage_stack(StorageStack::from_turso(store));
    let digest = format!("sha256:{}", "a".repeat(64));
    let request = |action: Option<&str>| {
        let path = action.map_or_else(
            || "/tdata/Orders".to_string(),
            |action| format!("/tdata/Orders('turso-continuity')/Temper.ScopedExample.{action}"),
        );
        Request::post(path)
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("x-temper-schema-scope-kind", "task")
            .header("x-temper-schema-scope-id", "task-router")
            .header("x-temper-schema-bundle-digest", digest.as_str())
            .body(Body::from(if action.is_none() {
                r#"{"Id":"turso-continuity"}"#
            } else {
                "{}"
            }))
            .unwrap()
    };
    let app = build_router(state.clone());
    assert_eq!(
        app.clone().oneshot(request(None)).await.unwrap().status(),
        StatusCode::CREATED
    );
    assert_eq!(
        app.oneshot(request(Some("Configure")))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    drop(state);

    let reopened = TursoEventStore::new(&database_url, None)
        .await
        .expect("reopen Turso store");
    let mut restarted = test_state_with_ioa();
    restarted.set_storage_stack(StorageStack::from_turso(reopened));
    assert_eq!(
        build_router(restarted)
            .oneshot(request(Some("Simulate")))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn task_scoped_bound_action_honors_exact_entity_pin_after_pointer_change() {
    const REPLACEMENT_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Ready"]
initial = "Draft"

[[action]]
name = "Configure"
kind = "input"
from = ["Draft"]
to = "Ready"
"#;

    let state = test_state_with_ioa();
    let tenant = TenantId::default();
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-router".into(),
    };
    let pinned_digest = format!("sha256:{}", "a".repeat(64));
    let replacement_digest = format!("sha256:{}", "b".repeat(64));
    let scoped_csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml")
        .replace("Temper.Example", "Temper.ScopedExample");
    let parsed = parse_csdl(&scoped_csdl).expect("scoped CSDL fixture");
    {
        let mut registry = state.registry.write().expect("registry lock");
        registry
            .stage_scoped_bundle(
                tenant.clone(),
                scope.clone(),
                pinned_digest.clone(),
                parsed.clone(),
                scoped_csdl.clone(),
                &[("Order", SCOPED_CONTINUITY_IOA)],
            )
            .expect("stage pinned bundle");
        registry
            .activate_scoped_bundle(&tenant, &scope, &pinned_digest, None)
            .expect("activate pinned bundle");
    }
    let app = build_router(state.clone());
    let scoped = |request: axum::http::request::Builder| {
        request
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("x-temper-schema-scope-kind", "task")
            .header("x-temper-schema-scope-id", "task-router")
            .header("x-temper-schema-bundle-digest", pinned_digest.as_str())
    };

    let create = app
        .clone()
        .oneshot(
            scoped(Request::post("/tdata/Orders"))
                .body(Body::from(r#"{"Id":"pin-route-order"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);
    let configure = app
        .clone()
        .oneshot(
            scoped(Request::post(
                "/tdata/Orders('pin-route-order')/Temper.ScopedExample.Configure",
            ))
            .body(Body::from("{}"))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(configure.status(), StatusCode::OK);

    {
        let mut registry = state.registry.write().expect("registry lock");
        registry
            .stage_scoped_bundle(
                tenant.clone(),
                scope.clone(),
                replacement_digest.clone(),
                parsed,
                scoped_csdl,
                &[("Order", REPLACEMENT_IOA)],
            )
            .expect("stage replacement bundle");
        registry
            .activate_scoped_bundle(&tenant, &scope, &replacement_digest, Some(&pinned_digest))
            .expect("activate replacement bundle");
    }

    let simulate = app
        .clone()
        .oneshot(
            scoped(Request::post(
                "/tdata/Orders('pin-route-order')/Temper.ScopedExample.Simulate",
            ))
            .body(Body::from("{}"))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(simulate.status(), StatusCode::OK);

    let internal_mismatch = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "pin-route-order",
            "Configure",
            serde_json::json!({}),
            &AgentContext {
                schema_pin: Some(SchemaExecutionPin {
                    scope: scope.clone(),
                    bundle_digest: replacement_digest.clone(),
                }),
                ..AgentContext::default()
            },
        )
        .await
        .expect_err("internal dispatch must reject a replacement pin for an existing entity");
    assert!(internal_mismatch.to_string().contains("SchemaPinMismatch"));

    let mismatched = app
        .oneshot(
            Request::post("/tdata/Orders('pin-route-order')/Temper.ScopedExample.Configure")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .header("x-temper-schema-bundle-digest", replacement_digest.as_str())
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mismatched.status(), StatusCode::CONFLICT);
    let mismatch_body = axum::body::to_bytes(mismatched.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let mismatch_json: serde_json::Value = serde_json::from_slice(&mismatch_body).unwrap();
    assert_eq!(mismatch_json["error"]["code"], "SchemaPinMismatch");

    let pointer_mismatch = build_router(state.clone())
        .oneshot(
            Request::post("/tdata/Orders('pin-route-order')/Temper.ScopedExample.Configure")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pointer_mismatch.status(), StatusCode::CONFLICT);
    let pointer_body = axum::body::to_bytes(pointer_mismatch.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let pointer_json: serde_json::Value = serde_json::from_slice(&pointer_body).unwrap();
    assert_eq!(pointer_json["error"]["code"], "SchemaPinMismatch");

    state
        .registry
        .write()
        .expect("registry lock")
        .retire_scoped_bundle(&tenant, &scope, &replacement_digest)
        .expect("retire replacement bundle");
    let retired_existing = build_router(state.clone())
        .oneshot(
            scoped(Request::post(
                "/tdata/Orders('pin-route-order')/Temper.ScopedExample.Simulate",
            ))
            .body(Body::from("{}"))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retired_existing.status(), StatusCode::OK);
    let retired_create = build_router(state)
        .oneshot(
            scoped(Request::post("/tdata/Orders"))
                .body(Body::from(r#"{"Id":"retired-new-order"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retired_create.status(), StatusCode::CONFLICT);
    let retired_body = axum::body::to_bytes(retired_create.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let retired_json: serde_json::Value = serde_json::from_slice(&retired_body).unwrap();
    assert_eq!(retired_json["error"]["code"], "SchemaPinMismatch");
}
