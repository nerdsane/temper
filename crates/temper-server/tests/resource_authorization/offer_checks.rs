use super::*;

fn register_governance(state: &ServerState) {
    use temper_server::registry::{EntityVerificationResult, VerificationStatus};
    let csdl = include_str!("../../../temper-platform/src/specs/model.csdl.xml");
    let ioa = include_str!("../../../temper-platform/src/specs/GovernanceDecision.ioa.toml");
    let mut registry = state.registry.write().unwrap();
    registry.register_tenant(
        "temper-system",
        temper_spec::csdl::parse_csdl(csdl).unwrap(),
        csdl.to_string(),
        &[("GovernanceDecision", ioa)],
    );
    registry.set_verification_status(
        &TenantId::new("temper-system"),
        "GovernanceDecision",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-10-26T00:00:00Z".to_string(),
        }),
    );
}

async fn answer(app: &axum::Router, body: serde_json::Value, tenant: &str) -> serde_json::Value {
    let mut request = json_request(Method::POST, "/api/authorize", body, tenant, "agent-1");
    let mut security = customer_security_context("agent-1");
    security.principal.kind = PrincipalKind::Agent;
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            security,
        ));
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

/// `order-1`, to which only a person may add an item; an agent may not even read it.
async fn order_only_a_person_may_add_to(state: &ServerState) {
    use temper_server::registry::{EntityVerificationResult, VerificationStatus};
    let csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    let ioa = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
    {
        let mut registry = state.registry.write().unwrap();
        registry.register_tenant(
            TENANT,
            temper_spec::csdl::parse_csdl(csdl).unwrap(),
            csdl.to_string(),
            &[("Order", ioa)],
        );
        registry.set_verification_status(
            &TenantId::new(TENANT),
            "Order",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-10-26T00:00:00Z".to_string(),
            }),
        );
    }
    state
        .get_or_create_tenant_entity(
            &TenantId::new(TENANT),
            "Order",
            "order-1",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"permit(principal is Customer, action == Action::"AddItem", resource is Order);"#,
        )
        .unwrap();
}

/// `request` as agent-1 inside an interactive session, the case that files decisions.
fn as_agent_in_session(mut request: Request<Body>) -> Request<Body> {
    request.headers_mut().insert(
        "x-session-id",
        axum::http::HeaderValue::from_static("session-1"),
    );
    let mut security = customer_security_context("agent-1");
    security.principal.kind = PrincipalKind::Agent;
    request.extensions_mut().insert(
        AuthenticatedRequestContext::new(TenantId::new(TENANT), security)
            .with_session_id(Some("session-1".to_string())),
    );
    request
}

/// Pending decisions, GovernanceDecision records and denial patterns filed so far.
async fn filed(state: &ServerState, store: &TursoEventStore) -> (usize, usize, usize) {
    (
        store.query_all_decisions(None).await.unwrap().len(),
        state
            .list_entity_ids(&TenantId::new("temper-system"), "GovernanceDecision")
            .len(),
        store
            .load_policy_denial_patterns(TENANT)
            .await
            .unwrap()
            .len(),
    )
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .expect("request should build")
}

/// The decision a refused request answered with, read from its 403 body.
async fn refused_with(response: axum::response::Response) -> String {
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    body["decision_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a refused request offers a decision: {body}"))
        .to_string()
}

/// An agent in a session asks 20 times whether it may add an item, which files
/// nothing. It then reads the record 20 times and is refused each time: the
/// first refusal files one decision and every retry gets that same decision
/// back. The move it then sends and is refused files one more.
#[tokio::test]
async fn questions_file_nothing_and_a_retried_refusal_reuses_its_waiting_decision() {
    let (state, store, _temp) = state_with_turso("retry-reuses-decision").await;
    register_governance(&state);
    order_only_a_person_may_add_to(&state).await;
    let app = build_router(state.clone());
    for _ in 0..20 {
        let offer = app
            .clone()
            .oneshot(as_agent_in_session(json_request(
                Method::POST,
                "/api/authorize",
                serde_json::json!({
                    "agent_id": "agent-1", "action": "AddItem", "resource_type": "Order",
                    "resource_id": "order-1", "context": {"status": "Draft"}
                }),
                TENANT,
                "agent-1",
            )))
            .await
            .unwrap();
        assert_eq!(offer.status(), StatusCode::OK);
    }
    assert_eq!(
        filed(&state, &store).await,
        (0, 0, 0),
        "a question filed a decision"
    );

    let mut read_decisions = std::collections::BTreeSet::new();
    for _ in 0..20 {
        let read = app
            .clone()
            .oneshot(as_agent_in_session(get("/tdata/Orders('order-1')")))
            .await
            .unwrap();
        read_decisions.insert(refused_with(read).await);
    }
    assert_eq!(
        read_decisions.len(),
        1,
        "every retry answers with the waiting decision"
    );
    assert_eq!(
        filed(&state, &store).await,
        (1, 1, 1),
        "20 refused reads of one record file one decision"
    );

    let refused = app
        .oneshot(as_agent_in_session(json_request(
            Method::POST,
            "/tdata/Orders('order-1')/Temper.AddItem",
            serde_json::json!({"ProductId": "p1", "Quantity": 1}),
            TENANT,
            "agent-1",
        )))
        .await
        .unwrap();
    let move_decision = refused_with(refused).await;
    assert!(!read_decisions.contains(&move_decision));
    assert_eq!(
        filed(&state, &store).await,
        (2, 2, 2),
        "a refused move is its own decision"
    );
}

#[tokio::test]
async fn factory_sized_offer_batch_matches_single_checks_without_filing_decisions() {
    let (state, store, _temp) = state_with_turso("offer-batch").await;
    register_governance(&state);
    // A record with 100 moves: agents may make the even moves, only a person may make the odd ones.
    let policy = (0..100).map(|i| format!(
        "permit(principal is {}, action == Action::\"Move{i}\", resource is Release) when {{ resource.owner == \"person-1\" }};",
        if i % 2 == 0 { "Agent" } else { "Customer" }
    )).collect::<Vec<_>>().join("\n");
    state.authz.reload_tenant_policies(TENANT, &policy).unwrap();
    state.authz.reload_tenant_policies("other", "").unwrap();
    let app = build_router(state.clone());
    let actions = (0..100).map(|i| format!("Move{i}")).collect::<Vec<_>>();
    let context = serde_json::json!({"owner": "person-1", "id": "spoofed"});
    let batch = answer(&app, serde_json::json!({
        "agent_id": "agent-1", "records": [
            {"resource_type": "Release", "resource_id": "release-1", "actions": actions, "context": context},
            {"resource_type": "Release", "resource_id": "release-2", "actions": ["Move0"], "context": {"owner":"other"}}
        ]
    }), TENANT).await;
    let records = batch["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["resource_id"], "release-1");
    let offers = records[0]["actions"].as_array().unwrap();
    assert_eq!(offers.len(), 100);
    for (i, offer) in offers.iter().enumerate() {
        let single = answer(
            &app,
            serde_json::json!({
                "agent_id":"agent-1", "resource_type":"Release", "resource_id":"release-1",
                "action":actions[i], "context":context
            }),
            TENANT,
        )
        .await;
        assert_eq!(offer["action"], actions[i]);
        assert_eq!(offer["allowed"], i % 2 == 0);
        assert_eq!(offer["allowed"], single["allowed"]);
        assert_eq!(offer["reason"], single["reason"]);
        assert!(offer.get("decision_id").is_none());
        if i % 2 != 0 {
            assert!(offer["reason"].as_str().is_some_and(|r| !r.is_empty()));
        }
    }
    assert_eq!(records[1]["actions"][0]["allowed"], false);
    let other = answer(&app, serde_json::json!({"agent_id":"agent-1", "records":[{
        "resource_type":"Release", "resource_id":"release-1", "actions":["Move0"], "context":context
    }]}), "other").await;
    assert_eq!(other["records"][0]["actions"][0]["allowed"], false);
    assert!(store.query_all_decisions(None).await.unwrap().is_empty());
    assert!(
        store
            .load_policy_denial_patterns(TENANT)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        state
            .list_entity_ids(&TenantId::new("temper-system"), "GovernanceDecision")
            .is_empty()
    );
}

#[tokio::test]
async fn refused_dispatch_still_files_exactly_one_decision_after_offer_checks() {
    let (state, store, _temp) = state_with_turso("offer-then-dispatch").await;
    register_governance(&state);
    order_only_a_person_may_add_to(&state).await;
    let app = build_router(state.clone());
    let offers = answer(&app, serde_json::json!({"agent_id":"agent-1", "records":[{
        "resource_type":"Order", "resource_id":"order-1", "actions":["AddItem"], "context":{"status":"Draft"}
    }]}), TENANT).await;
    assert_eq!(offers["records"][0]["actions"][0]["allowed"], false);
    assert!(
        store
            .query_decisions(TENANT, None)
            .await
            .unwrap()
            .is_empty()
    );
    let mut request = json_request(
        Method::POST,
        "/tdata/Orders('order-1')/Temper.AddItem",
        serde_json::json!({"ProductId":"p1", "Quantity":1}),
        TENANT,
        "agent-1",
    );
    let mut security = customer_security_context("agent-1");
    security.principal.kind = PrincipalKind::Agent;
    // An agent in a session, which waits for the decision: only such a
    // refusal files one (`authz::denial_can_be_answered`).
    request.headers_mut().insert(
        "x-session-id",
        axum::http::HeaderValue::from_static("session-1"),
    );
    request.extensions_mut().insert(
        AuthenticatedRequestContext::new(TenantId::new(TENANT), security)
            .with_session_id(Some("session-1".to_string())),
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    let decisions = store.query_decisions(TENANT, None).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        state
            .list_entity_ids(&TenantId::new("temper-system"), "GovernanceDecision")
            .len(),
        1
    );
    let decision: serde_json::Value = serde_json::from_str(&decisions[0]).unwrap();
    assert_eq!(body["decision_id"], decision["id"]);
    assert_eq!(
        store
            .load_policy_denial_patterns(TENANT)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn offer_batches_validate_the_whole_request_and_authenticated_principal() {
    let (state, store, _temp) = state_with_turso("offer-batch-validation").await;
    let app = build_router(state);
    for (body, expected) in [
        (
            serde_json::json!({"agent_id":"intruder", "records":[]}),
            StatusCode::FORBIDDEN,
        ),
        (
            serde_json::json!({"agent_id":"reader", "records":[
                {"resource_type":"Release", "resource_id":"r1", "actions":["Approve"]},
                {"resource_type":"Release", "resource_id":"r2", "actions":["Approve"], "context":42}
            ]}),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"agent_id":"reader", "records":[], "action":"Approve", "resource_type":"Release", "resource_id":"r1"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            serde_json::json!({"agent_id":"reader", "records":[{"resource_type":"Release", "resource_id":"r1", "actions":[], "context":{"large":"x".repeat(2 * 1024 * 1024)}}]}),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/authorize",
                body,
                TENANT,
                "reader",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    assert!(store.query_all_decisions(None).await.unwrap().is_empty());
    assert!(
        store
            .load_policy_denial_patterns(TENANT)
            .await
            .unwrap()
            .is_empty()
    );
    let empty = answer(
        &app,
        serde_json::json!({"agent_id":"agent-1", "records":[]}),
        TENANT,
    )
    .await;
    assert_eq!(empty, serde_json::json!({"records":[]}));
}
