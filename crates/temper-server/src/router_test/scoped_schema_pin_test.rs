//! Regression coverage for task-scoped schema-pin routing and restart continuity.

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
async fn task_scoped_action_validation_uses_the_exact_pinned_csdl() {
    const IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Ready"]
initial = "Draft"

[[action]]
name = "AddItem"
kind = "input"
from = ["Draft"]
to = "Ready"
params = [{ name = "Flag", type = "bool" }]
"#;
    let mut state = test_state_with_ioa();
    let tenant = TenantId::default();
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "requiredness-pin".into(),
    };
    let digest = format!("sha256:{}", "c".repeat(64));
    let original_action = r#"      <Action Name="AddItem" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Example.Order" Nullable="false"/>
        <Parameter Name="ProductId" Type="Edm.Guid" Nullable="false"/>
        <Parameter Name="Quantity" Type="Edm.Int32" Nullable="false"/>
        <ReturnType Type="Temper.Example.Order"/>
        <Annotation Term="Temper.Vocab.StateMachine.ValidFromStates">
          <Collection><String>Draft</String></Collection>
        </Annotation>
        <Annotation Term="Temper.Vocab.Agent.Hint"
                    String="Add a product to a draft order. Verify the order is in Draft status first. Use the Product entity to look up valid ProductIds."/>
      </Action>"#;
    let pinned_action = r#"      <Action Name="AddItem" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.ScopedExample.Order" Nullable="false"/>
        <Parameter Name="Flag" Type="Edm.Boolean" Nullable="false"/>
      </Action>"#;
    let scoped_csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml")
        .replace(original_action, pinned_action)
        .replace("Temper.Example", "Temper.ScopedExample");
    assert!(scoped_csdl.contains("<Parameter Name=\"Flag\""));
    let store = SimEventStore::no_faults(9_193);
    persist_task_schema_bundle_with_csdl_in_scope(
        &store,
        IOA,
        scoped_csdl.clone(),
        &digest,
        None,
        "requiredness-pin",
        &scope,
    )
    .await;
    state.set_storage_stack(StorageStack::from_sim(store, None));
    {
        let mut registry = state.registry.write().expect("registry lock");
        registry
            .stage_scoped_bundle(
                tenant.clone(),
                scope.clone(),
                digest.clone(),
                parse_csdl(&scoped_csdl).expect("scoped requiredness CSDL"),
                scoped_csdl,
                &[("Order", IOA)],
            )
            .expect("stage requiredness bundle");
        registry
            .activate_scoped_bundle(&tenant, &scope, &digest, None)
            .expect("activate requiredness bundle");
    }
    let app = authenticated_router(state.clone());
    let scoped = |request: axum::http::request::Builder| {
        request
            .header("Content-Type", "application/json")
            .header("X-Temper-Principal-Kind", "admin")
            .header("x-temper-schema-scope-kind", "task")
            .header("x-temper-schema-scope-id", scope.id.as_str())
            .header("x-temper-schema-bundle-digest", digest.as_str())
    };
    let create = app
        .clone()
        .oneshot(
            scoped(Request::post("/tdata/Orders"))
                .body(Body::from(r#"{"Id":"pinned-validation"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);

    for (body, expected_code) in [
        (r#"{"Flag":"not-a-boolean"}"#, "ActionParameterTypeMismatch"),
        (r#"{}"#, "MissingActionParameter"),
        (r#"{"Flag":null}"#, "MissingActionParameter"),
    ] {
        let response = app
            .clone()
            .oneshot(
                scoped(Request::post(
                    "/tdata/Orders('pinned-validation')/Temper.ScopedExample.AddItem",
                ))
                .body(Body::from(body))
                .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(json["error"]["code"], expected_code);
    }
    let pin = SchemaExecutionPin {
        scope,
        bundle_digest: digest,
    };
    assert_eq!(
        state
            .get_scoped_entity_state(&tenant, "Order", "pinned-validation", pin)
            .await
            .expect("rejected actions leave the entity readable")
            .state
            .status,
        "Draft"
    );
}

#[tokio::test]
async fn canonical_scoped_journal_id_is_reserved_from_global_actor_and_journal_use() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::persistence::schema_deployment::scoped_journal_entity_id;

    let (state, store) =
        test_state_with_durable_active_task_schema_and_ioa(SCOPED_CONTINUITY_IOA).await;
    let tenant = TenantId::default();
    let pin = SchemaExecutionPin {
        scope: SchemaScope {
            kind: SchemaScopeKind::Task,
            id: "task-router".into(),
        },
        bundle_digest: format!("sha256:{}", "a".repeat(64)),
    };
    state
        .get_or_create_scoped_entity(
            &tenant,
            "Order",
            "foo",
            serde_json::json!({"scope_marker": "scoped"}),
            pin.clone(),
        )
        .await
        .expect("create scoped entity");

    let reserved_id = scoped_journal_entity_id("foo", &pin);
    let persistence_id = format!("{tenant}:Order:{reserved_id}");
    let actor_count_before = state.actor_registry.read().expect("actor registry").len();
    let journal_before = store
        .read_events(&persistence_id, 0)
        .await
        .expect("read scoped journal before collision attempt");

    let error = state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            &reserved_id,
            serde_json::json!({"scope_marker": "global"}),
        )
        .await
        .expect_err("global entity must not occupy scoped-journal namespace");
    assert!(error.contains("No transition table"));
    assert_eq!(
        state.actor_registry.read().expect("actor registry").len(),
        actor_count_before,
        "global collision attempt must not spawn or alias an actor"
    );
    assert_eq!(
        store
            .read_events(&persistence_id, 0)
            .await
            .expect("read scoped journal after collision attempt"),
        journal_before,
        "global collision attempt must not append to the scoped journal"
    );
    let scoped = state
        .get_scoped_entity_state(&tenant, "Order", "foo", pin)
        .await
        .expect("scoped entity remains readable");
    assert_eq!(scoped.state.fields["scope_marker"], "scoped");
}

#[tokio::test]
async fn canonical_scoped_journal_id_is_reserved_from_data_only_direct_append() {
    use temper_runtime::persistence::EventStore;
    use temper_runtime::persistence::schema_deployment::scoped_journal_entity_id;

    let (state, store) = test_state_with_data_only_ioa_and_sim().await;
    let tenant = TenantId::default();
    let pin = SchemaExecutionPin {
        scope: SchemaScope {
            kind: SchemaScopeKind::Task,
            id: "task-router".into(),
        },
        bundle_digest: format!("sha256:{}", "a".repeat(64)),
    };
    let reserved_id = scoped_journal_entity_id("foo", &pin);
    let persistence_id = format!("{tenant}:LogEntry:{reserved_id}");

    let error = state
        .try_create_data_only_tenant_entity(
            &tenant,
            "LogEntry",
            &reserved_id,
            serde_json::json!({"Id": reserved_id, "Body": "must not persist"}),
        )
        .await
        .expect_err("data-only global entity must not occupy scoped-journal namespace");
    assert!(error.contains("reserved scoped-journal identity form"));
    assert!(
        store
            .read_events(&persistence_id, 0)
            .await
            .expect("read reserved journal after collision attempt")
            .is_empty(),
        "data-only collision attempt must not append to the scoped journal"
    );
    assert!(
        !state.entity_exists(&tenant, "LogEntry", &reserved_id),
        "data-only collision attempt must not update the global projection index"
    );
}

#[tokio::test]
async fn identical_digest_in_distinct_scopes_has_distinct_actor_and_journal_authority() {
    use temper_runtime::persistence::EventStore;

    let (state, store) =
        test_state_with_durable_active_task_schema_and_ioa(SCOPED_CONTINUITY_IOA).await;
    let tenant = TenantId::default();
    let digest = format!("sha256:{}", "a".repeat(64));
    let scope_a = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-router".into(),
    };
    let scope_b = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-router-b".into(),
    };
    persist_task_schema_bundle_in_scope(
        &store,
        SCOPED_CONTINUITY_IOA,
        &digest,
        None,
        "same-digest-scope-b",
        &scope_b,
    )
    .await;
    let scoped_csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml")
        .replace("Temper.Example", "Temper.ScopedExample");
    {
        let mut registry = state.registry.write().expect("registry lock");
        registry
            .stage_scoped_bundle(
                tenant.clone(),
                scope_b.clone(),
                digest.clone(),
                parse_csdl(&scoped_csdl).expect("scope B CSDL"),
                scoped_csdl,
                &[("Order", SCOPED_CONTINUITY_IOA)],
            )
            .expect("stage same digest in scope B");
        registry
            .activate_scoped_bundle(&tenant, &scope_b, &digest, None)
            .expect("activate same digest in scope B");
    }
    let pin_a = SchemaExecutionPin {
        scope: scope_a.clone(),
        bundle_digest: digest.clone(),
    };
    let pin_b = SchemaExecutionPin {
        scope: scope_b.clone(),
        bundle_digest: digest.clone(),
    };
    state
        .get_or_create_scoped_entity(
            &tenant,
            "Order",
            "same-id",
            serde_json::json!({"scope_marker": "a"}),
            pin_a.clone(),
        )
        .await
        .expect("create scope A entity");
    state
        .get_or_create_scoped_entity(
            &tenant,
            "Order",
            "same-id",
            serde_json::json!({"scope_marker": "b"}),
            pin_b.clone(),
        )
        .await
        .expect("create scope B entity");

    let state_a = state
        .get_scoped_entity_state(&tenant, "Order", "same-id", pin_a)
        .await
        .expect("load scope A entity");
    let state_b = state
        .get_scoped_entity_state(&tenant, "Order", "same-id", pin_b)
        .await
        .expect("load scope B entity");
    assert_eq!(state_a.state.fields["scope_marker"], "a");
    assert_eq!(state_b.state.fields["scope_marker"], "b");
    assert_eq!(
        store
            .scoped_entity_bundle_digests(tenant.as_str(), "Order", "same-id", &scope_a, 2,)
            .await
            .expect("scope A durable pin"),
        vec![digest.clone()]
    );
    assert_eq!(
        store
            .scoped_entity_bundle_digests(tenant.as_str(), "Order", "same-id", &scope_b, 2,)
            .await
            .expect("scope B durable pin"),
        vec![digest]
    );
}

#[tokio::test]
async fn colon_bearing_entity_pin_does_not_authorize_its_prefix_entity() {
    let (state, _store) =
        test_state_with_durable_active_task_schema_and_ioa(SCOPED_CONTINUITY_IOA).await;
    let digest = format!("sha256:{}", "a".repeat(64));
    let pin = SchemaExecutionPin {
        scope: SchemaScope {
            kind: SchemaScopeKind::Task,
            id: "task-router".into(),
        },
        bundle_digest: digest.clone(),
    };
    let base_id = "collision-base";
    let colon_id = format!("{base_id}:schema:{digest}");
    state
        .get_or_create_scoped_entity(
            &TenantId::default(),
            "Order",
            &colon_id,
            serde_json::json!({"Id": colon_id}),
            pin.clone(),
        )
        .await
        .expect("create colon-bearing scoped entity");

    let error = state
        .get_scoped_entity_state(&TenantId::default(), "Order", base_id, pin)
        .await
        .expect_err("prefix entity must not inherit the colon-bearing entity pin");
    assert!(error.contains("has no durable pin"));
}

#[tokio::test]
async fn volatile_actor_does_not_grant_durable_pin_authority() {
    let state = test_state_with_active_task_schema_and_ioa(SCOPED_CONTINUITY_IOA);
    let tenant = TenantId::default();
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-router".into(),
    };
    let source = format!("sha256:{}", "a".repeat(64));
    let replacement = format!("sha256:{}", "b".repeat(64));
    let source_pin = SchemaExecutionPin {
        scope: scope.clone(),
        bundle_digest: source.clone(),
    };
    state
        .get_or_create_scoped_entity(
            &tenant,
            "Order",
            "volatile-only",
            serde_json::json!({"Id": "volatile-only"}),
            source_pin.clone(),
        )
        .await
        .expect("spawn volatile scoped actor");
    let scoped_csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml")
        .replace("Temper.Example", "Temper.ScopedExample");
    let parsed = parse_csdl(&scoped_csdl).expect("scoped CSDL fixture");
    {
        let mut registry = state.registry.write().expect("registry lock");
        registry
            .stage_scoped_bundle(
                tenant.clone(),
                scope.clone(),
                replacement.clone(),
                parsed,
                scoped_csdl,
                &[("Order", SCOPED_CONTINUITY_IOA)],
            )
            .expect("stage replacement bundle");
        registry
            .activate_scoped_bundle(&tenant, &scope, &replacement, Some(&source))
            .expect("activate replacement bundle");
    }

    let error = state
        .get_scoped_entity_state(&tenant, "Order", "volatile-only", source_pin)
        .await
        .expect_err("volatile actor must not establish durable authority");
    assert!(error.contains("has no durable pin"));
}

#[tokio::test]
async fn task_scoped_read_requires_a_durable_entity_pin() {
    let digest = format!("sha256:{}", "a".repeat(64));
    let response = authenticated_router(test_state_with_active_task_schema_and_ioa(
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
    let app = authenticated_router(state.clone());
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
        authenticated_router(restarted)
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
    let app = authenticated_router(state.clone());
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
        authenticated_router(restarted)
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

    let (state, store) =
        test_state_with_durable_active_task_schema_and_ioa(SCOPED_CONTINUITY_IOA).await;
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
    let app = authenticated_router(state.clone());
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

    persist_task_schema_bundle(
        &store,
        REPLACEMENT_IOA,
        &replacement_digest,
        Some(&pinned_digest),
        "replacement",
    )
    .await;

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

    let scope_only_navigation = app
        .clone()
        .oneshot(
            Request::get("/tdata/Orders('pin-route-order')/Payment")
                .header("X-Temper-Principal-Kind", "admin")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(scope_only_navigation.status(), StatusCode::OK);

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

    let scope_only_existing = authenticated_router(state.clone())
        .oneshot(
            Request::post("/tdata/Orders('pin-route-order')/Temper.ScopedExample.Simulate")
                .header("Content-Type", "application/json")
                .header("X-Temper-Principal-Kind", "admin")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "task-router")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(scope_only_existing.status(), StatusCode::OK);

    state
        .registry
        .write()
        .expect("registry lock")
        .retire_scoped_bundle(&tenant, &scope, &replacement_digest)
        .expect("retire replacement bundle");
    let retired_existing = authenticated_router(state.clone())
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
    let retired_create = authenticated_router(state)
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
