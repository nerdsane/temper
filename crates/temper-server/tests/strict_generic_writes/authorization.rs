use super::*;

#[tokio::test]
async fn pg_posts_report_unavailable_runtime_without_creating_native_entities() {
    let mut state = state();
    state.actor_backed_types.insert("Order".into());
    for path in [
        "/tdata/Orders",
        "/tdata/Orders('missing')/Temper.SubmitOrder",
    ] {
        let response = request(&state, "POST", path, json!({})).await;
        assert_eq!(response, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert!(!state.entity_exists(&TenantId::default(), "Order", "missing"));
        assert_eq!(state.active_actor_count(), 0);
    }
}

#[tokio::test]
async fn unauthorized_writes_do_not_reveal_verification_status() {
    let csdl = common::CSDL_XML.replace(
        "<EntityType Name=\"Order\">",
        "<EntityType Name=\"Order\" HasStream=\"true\">",
    );
    let state = state_with_csdl(&csdl);
    assert_eq!(
        request(&state, "POST", "/tdata/Orders", json!({"id":"valid"})).await,
        StatusCode::CREATED
    );
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Order",
        VerificationStatus::Pending,
    );
    for permitted in [false, true] {
        state
            .authz
            .reload_tenant_policies(
                "default",
                if permitted {
                    "permit(principal, action, resource);"
                } else {
                    "forbid(principal, action, resource);"
                },
            )
            .unwrap();
        for (method, path) in [
            ("POST", "/tdata/Orders('valid')/Temper.SubmitOrder"),
            ("PATCH", "/tdata/Orders('valid')"),
            ("PUT", "/tdata/Orders('valid')"),
            ("PUT", "/tdata/Orders('valid')/$value"),
            ("DELETE", "/tdata/Orders('valid')"),
        ] {
            assert_eq!(
                request(&state, method, path, json!({})).await,
                if permitted {
                    StatusCode::LOCKED
                } else {
                    StatusCode::FORBIDDEN
                },
                "{method} {path}"
            );
        }
    }
}

#[tokio::test]
async fn refused_missing_actions_leave_no_actor_or_journal_entry() {
    use temper_runtime::persistence::EventStore;
    for allowed in [false, true] {
        let mut state = state();
        let store = temper_store_sim::SimEventStore::no_faults(467);
        state.set_storage_stack(temper_server::StorageStack::from_sim(store.clone(), None));
        state.registry.write().unwrap().set_verification_status(
            &TenantId::default(),
            "Order",
            VerificationStatus::Pending,
        );
        state
            .authz
            .reload_tenant_policies(
                "default",
                if allowed {
                    "permit(principal, action, resource);"
                } else {
                    "forbid(principal, action, resource);"
                },
            )
            .unwrap();
        assert_eq!(
            request(
                &state,
                "POST",
                "/tdata/Orders('never-created')/Temper.SubmitOrder",
                json!({})
            )
            .await,
            if allowed {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::FORBIDDEN
            }
        );
        assert_eq!(state.active_actor_count(), 0);
        assert!(!state.entity_exists(&TenantId::default(), "Order", "never-created"));
        assert!(
            store
                .read_events("default:Order:never-created", 0)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .load_snapshot("default:Order:never-created")
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn explicit_creation_and_cold_recovery_use_actual_authorization_state() {
    use temper_runtime::persistence::EventStore;
    let store = temper_store_sim::SimEventStore::no_faults(467);
    let mut initial = state();
    initial.set_storage_stack(temper_server::StorageStack::from_sim(store.clone(), None));
    assert_eq!(
        request(&initial, "POST", "/tdata/Orders", json!({"id":"explicit"})).await,
        StatusCode::CREATED
    );
    assert_eq!(
        request(
            &initial,
            "POST",
            "/tdata/Orders('explicit')/Temper.SubmitOrder",
            json!({"Notes":"persisted"})
        )
        .await,
        StatusCode::OK
    );
    let before = store
        .read_events("default:Order:explicit", 0)
        .await
        .unwrap();
    assert!(!before.is_empty());
    let mut recovered = state();
    recovered.set_storage_stack(temper_server::StorageStack::from_sim(store.clone(), None));
    recovered
        .authz
        .reload_tenant_policies(
            "default",
            "permit(principal, action, resource) when { resource.Status == \"Draft\" };",
        )
        .unwrap();
    assert_eq!(recovered.active_actor_count(), 0);
    assert_eq!(
        request(
            &recovered,
            "POST",
            "/tdata/Orders('explicit')/Temper.SubmitOrder",
            json!({"Notes":"forged"})
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let after = store
        .read_events("default:Order:explicit", 0)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap()
    );
}

#[tokio::test]
async fn first_generic_stream_upload_commits_fields_and_rejected_reupload_is_not_success() {
    let csdl = common::CSDL_XML.replace(
        "<EntityType Name=\"Order\">",
        "<EntityType Name=\"Order\" HasStream=\"true\">",
    );
    let mut state = state_with_csdl(&csdl);
    let store = temper_store_sim::SimEventStore::no_faults(467);
    state.set_storage_stack(temper_server::StorageStack::from_sim(store, None));
    let payload =
        json!({"action":"SubmitOrder","params":{"Notes":"uploaded"},"success":true}).to_string();
    let data = payload
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    let wat = format!(
        r#"(module
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{data}")
        (func (export "run") (param i32 i32) (result i32)
          i32.const 0 i32.const {} call $result i32.const 0))"#,
        payload.len()
    );
    let hash = state.wasm_engine.compile_and_cache(wat.as_bytes()).unwrap();
    state.wasm_module_registry.write().unwrap().register(
        &TenantId::default(),
        "blob_adapter",
        &hash,
    );
    let response = raw_request(
        &state,
        "PUT",
        "/tdata/Orders('upload')/$value",
        "payload".into(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let actual = state
        .get_tenant_entity_state(&TenantId::default(), "Order", "upload")
        .await
        .unwrap();
    assert_eq!(actual.state.status, "Submitted");
    assert_eq!(actual.state.fields["Notes"], "uploaded");
    let refused = raw_request(
        &state,
        "PUT",
        "/tdata/Orders('upload')/$value",
        "payload".into(),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn missing_commons_owner_refuses_for_explicitly_created_target() {
    let state = state();
    state.registry.write().unwrap().register_tenant(
        "default",
        parse_csdl(common::CSDL_XML).unwrap(),
        common::CSDL_XML.into(),
        &[
            ("Order", SPEC),
            (
                "Owner",
                r#"[automaton]
name="Owner"
states=["Unverified","Verified"]
initial="Unverified"
[[action]]
name="Verify"
from=["Unverified"]
to="Verified"
"#,
            ),
        ],
    );
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Order",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-09-08T00:00:00Z".into(),
        }),
    );
    assert_eq!(
        request(&state, "POST", "/tdata/Orders", json!({"id":"created"})).await,
        StatusCode::CREATED
    );
    state
        .commons_guardrail_tenants
        .write()
        .unwrap()
        .insert("default".into());
    let response = request(
        &state,
        "POST",
        "/tdata/Orders('created')/Temper.SubmitOrder",
        json!({"OwnerId":"missing-owner"}),
    )
    .await;
    assert_eq!(response, StatusCode::FORBIDDEN);
    assert_eq!(state.active_actor_count(), 1);
    assert!(!state.entity_exists(&TenantId::default(), "Owner", "missing-owner"));
    let order = state
        .get_tenant_entity_state(&TenantId::default(), "Order", "created")
        .await
        .unwrap();
    assert_eq!(order.state.status, "Draft");
}

#[tokio::test]
async fn authorized_collection_creation_rejects_noninitial_status() {
    let state = state();
    assert_eq!(
        request(
            &state,
            "POST",
            "/tdata/Orders",
            json!({"id":"invalid-status","Status":"Submitted"})
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(state.active_actor_count(), 0);
}
