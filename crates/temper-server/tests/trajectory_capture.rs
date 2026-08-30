//! End-to-end trajectory capture over the HTTP surface.
//!
//! Covers the two properties a JCS trajectory consumer depends on:
//!
//! 1. A **successful** governed action produces a durable trajectory row that
//!    carries its `request_body` — not only failures, which is all the capture
//!    path used to record.
//! 2. `X-Session-Id` and `X-Intent` travel from the HTTP request all the way
//!    into the persisted row, on both the success and the failure path.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::build_router;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::state::TrajectoryEntry;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = common::CSDL_XML;
const ORDER_IOA: &str = common::ORDER_IOA;

const SESSION_ID: &str = "sess-jcs-e2e";
const INTENT: &str = "add a line item to the draft order";

/// Build a Turso-backed state so trajectory rows land in a real sink.
///
/// The sim store has no trajectory capability, so a durable backend is the
/// only way to assert on what was actually persisted.
fn build_turso_state(system_name: &str, store: TursoEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl_xml = CSDL_XML.replacen(
        r#"<Parameter Name="Quantity" Type="Edm.Int32" Nullable="false"/>"#,
        r#"<Parameter Name="Quantity" Type="Edm.Int32" Nullable="false"/>
        <Parameter Name="Notes" Type="Edm.String"/>
        <Parameter Name="api_token" Type="Edm.String"/>
        <Parameter Name="payment" Type="Edm.Untyped"/>"#,
        1,
    );
    let order_ioa = ORDER_IOA.replacen(
        r#"params = ["ProductId", "Quantity"]"#,
        r#"params = [
  "ProductId",
  "Quantity",
  { name = "Notes", type = "Edm.String", nullable = true },
  { name = "api_token", type = "Edm.String", nullable = true },
  { name = "payment", type = "Edm.Untyped", nullable = true },
]"#,
        1,
    );
    let csdl = parse_csdl(&csdl_xml).expect("CSDL parse");
    registry.register_tenant("default", csdl, csdl_xml, &[("Order", &order_ioa)]);

    let state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    {
        let mut registry = state.registry.write().unwrap();
        registry.set_verification_status(
            &TenantId::default(),
            "Order",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "OK".to_string(),
                    details: None,
                }],
                verified_at: "2026-08-11T00:00:00Z".to_string(),
            }),
        );
    }

    let mut state = state;
    // ARN-170 hardened `from_registry`'s default engine to default-deny
    // (`AuthzEngine::empty()`); these capture/telemetry tests exercise the write
    // path itself, not authorization, so install a permissive tenant policy —
    // the effective posture they were written against. Tests that need a denial
    // (e.g. cedar_denied_action) reload a restrictive policy over this.
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            r#"permit(principal, action, resource);"#,
        )
        .expect("install permissive test policy");
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

async fn temp_store(label: &str) -> (TursoEventStore, std::path::PathBuf) {
    let db_path = std::env::temp_dir().join(format!("temper-{label}-{}.db", uuid::Uuid::new_v4()));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    (store, db_path)
}

/// Attach the credential context the ingress edge installs in production
/// (ADR-0157). These fixtures carry no principal headers, so the anonymous
/// Customer is exactly what the pre-edge header path produced; the
/// `X-Session-Id`/`X-Intent` correlation headers still travel on the request
/// and are read by the odata dispatch path.
fn with_test_auth(mut request: Request<Body>) -> Request<Body> {
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::anonymous(),
        ));
    request
}

/// POST with the observability headers under test.
async fn post_observed(
    state: &ServerState,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = with_test_auth(
        Request::post(path)
            .header("Content-Type", "application/json")
            .header("X-Session-Id", SESSION_ID)
            .header("X-Intent", INTENT)
            .body(Body::from(body.to_string()))
            .unwrap(),
    );
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let parsed = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, parsed)
}

/// Wait for a persisted trajectory row matching `predicate`.
///
/// Trajectory persistence is a background outbox (ADR-0067), so the row lands
/// after the HTTP response returns.
async fn await_trajectory(
    state: &ServerState,
    label: &str,
    predicate: impl Fn(&TrajectoryEntry) -> bool,
) -> TrajectoryEntry {
    for _ in 0..200 {
        if let Some(found) = state
            .load_trajectory_entries(TenantId::default().as_str(), 200)
            .await
            .into_iter()
            .find(&predicate)
        {
            return found;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let seen: Vec<String> = state
        .load_trajectory_entries(TenantId::default().as_str(), 200)
        .await
        .into_iter()
        .map(|e| format!("{}.{} success={}", e.entity_type, e.action, e.success))
        .collect();
    panic!("no trajectory row matched '{label}'; rows seen: {seen:?}");
}

#[tokio::test]
async fn successful_governed_action_persists_request_body_session_and_intent() {
    let (store, db_path) = temp_store("trajectory-success").await;
    let state = build_turso_state("trajectory-capture-success", store);

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders",
        serde_json::json!({"id": "ord-jcs-1", "Currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-1')/Temper.AddItem",
        serde_json::json!({"ProductId": "00000000-0000-0000-0000-000000000009", "Quantity": 3}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "AddItem failed: {body:?}");

    let entry = await_trajectory(&state, "successful AddItem", |entry| {
        entry.action == "AddItem" && entry.entity_id == "ord-jcs-1" && entry.success
    })
    .await;

    assert!(entry.success, "the captured row is the successful action");
    assert_eq!(
        entry.session_id.as_deref(),
        Some(SESSION_ID),
        "X-Session-Id must reach the persisted trajectory row"
    );
    assert_eq!(
        entry.intent.as_deref(),
        Some(INTENT),
        "X-Intent must reach the persisted trajectory row"
    );

    let request_body = entry
        .request_body
        .as_ref()
        .expect("successful actions must persist their request body");
    assert_eq!(
        request_body["ProductId"],
        serde_json::json!("00000000-0000-0000-0000-000000000009")
    );
    assert_eq!(request_body["Quantity"], serde_json::json!(3));

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn failed_governed_action_still_persists_request_body_session_and_intent() {
    let (store, db_path) = temp_store("trajectory-failure").await;
    let state = build_turso_state("trajectory-capture-failure", store);

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders",
        serde_json::json!({"id": "ord-jcs-2", "Currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    // SubmitOrder guards on `items > 0`; the fresh order has none, so the
    // guard rejects and the dispatch records a failed intent.
    let (status, _body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-2')/Temper.SubmitOrder",
        serde_json::json!({"ShippingAddressId": "10000000-0000-0000-0000-000000000001", "PaymentMethod": "card"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "SubmitOrder without items must fail the guard"
    );

    let entry = await_trajectory(&state, "failed SubmitOrder", |entry| {
        entry.action == "SubmitOrder" && entry.entity_id == "ord-jcs-2" && !entry.success
    })
    .await;

    assert_eq!(entry.session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(entry.intent.as_deref(), Some(INTENT));
    let request_body = entry
        .request_body
        .as_ref()
        .expect("failed actions keep persisting their request body");
    assert_eq!(
        request_body["ShippingAddressId"],
        serde_json::json!("10000000-0000-0000-0000-000000000001")
    );
    assert!(entry.error.is_some(), "the failure reason is recorded");

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn observe_prefixed_headers_are_honoured_as_session_and_intent() {
    // The canonical spellings are the `X-Temper-Observe-*` headers; the short
    // `X-Session-Id`/`X-Intent` forms are aliases. Both must land identically.
    let (store, db_path) = temp_store("trajectory-observe-headers").await;
    let state = build_turso_state("trajectory-capture-observe-headers", store);

    let router = build_router(state.clone());
    let resp = router
        .oneshot(with_test_auth(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::json!({"id": "ord-jcs-3", "Currency": "USD"}).to_string(),
                ))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let router = build_router(state.clone());
    let resp = router
        .oneshot(with_test_auth(
            Request::post("/tdata/Orders('ord-jcs-3')/Temper.AddItem")
                .header("Content-Type", "application/json")
                .header("X-Temper-Observe-Session-Id", "sess-observe-prefixed")
                .header("X-Temper-Observe-Intent", "observe-prefixed intent")
                .body(Body::from(
                    serde_json::json!({"ProductId": "00000000-0000-0000-0000-000000000003", "Quantity": 1}).to_string(),
                ))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let entry = await_trajectory(&state, "observe-prefixed AddItem", |entry| {
        entry.action == "AddItem" && entry.entity_id == "ord-jcs-3" && entry.success
    })
    .await;

    assert_eq!(
        entry.session_id.as_deref(),
        Some("sess-observe-prefixed"),
        "X-Temper-Observe-Session-Id must reach the persisted row"
    );
    assert_eq!(
        entry.intent.as_deref(),
        Some("observe-prefixed intent"),
        "X-Temper-Observe-Intent must reach the persisted row"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn cedar_denied_action_persists_intent_and_evaluated_attributes() {
    // An authorization denial is the row the Evolution Engine reasons over.
    // Without the caller's intent and the attributes Cedar actually saw, the
    // denial says what was blocked but not what the agent was attempting.
    let (store, db_path) = temp_store("trajectory-denial").await;
    let state = build_turso_state("trajectory-capture-denial", store);

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders",
        serde_json::json!({"id": "ord-jcs-4", "Currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            r#"permit(principal, action in [Action::"list", Action::"read"], resource is Order);"#,
        )
        .expect("install Cedar policy");

    let (status, _body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-4')/Temper.AddItem",
        serde_json::json!({"ProductId": "00000000-0000-0000-0000-000000000004", "Quantity": 1}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "AddItem must be denied under a read-only policy set"
    );

    let entry = await_trajectory(&state, "denied AddItem", |entry| {
        entry.action == "AddItem"
            && entry.entity_id == "ord-jcs-4"
            && entry.authz_denied == Some(true)
    })
    .await;

    assert_eq!(entry.intent.as_deref(), Some(INTENT));
    let request_body = entry
        .request_body
        .as_ref()
        .expect("denials must persist the attributes Cedar evaluated");
    assert_eq!(
        request_body["id"],
        serde_json::json!("ord-jcs-4"),
        "the evaluated resource attributes are recorded"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn oversized_request_body_is_bounded_before_persistence() {
    // Capturing every successful action makes large bodies routine rather than
    // rare. The stored row must stay bounded and stay parseable JSON — a
    // byte-sliced prefix would read back as no body at all.
    let (store, db_path) = temp_store("trajectory-oversized").await;
    let state = build_turso_state("trajectory-capture-oversized", store);

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders",
        serde_json::json!({"id": "ord-jcs-5", "Currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-5')/Temper.AddItem",
        serde_json::json!({"ProductId": "00000000-0000-0000-0000-000000000005", "Quantity": 1, "Notes": "x".repeat(50_000)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "AddItem failed: {body:?}");

    let entry = await_trajectory(&state, "oversized AddItem", |entry| {
        entry.action == "AddItem" && entry.entity_id == "ord-jcs-5" && entry.success
    })
    .await;

    let request_body = entry
        .request_body
        .as_ref()
        .expect("an oversized body must still round-trip as parseable JSON, not vanish");
    assert_eq!(
        request_body["_truncated"],
        serde_json::json!(true),
        "the stored row declares that it was truncated: {request_body}"
    );
    assert!(
        request_body["_original_bytes"]
            .as_u64()
            .expect("original bytes")
            > 50_000,
        "the envelope records the pre-truncation size"
    );
    assert!(
        request_body.to_string().len() <= 4096,
        "the stored body respects the cap"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn secret_named_parameters_are_redacted_before_persistence() {
    // Every successful action now records its arguments, so an action that
    // takes a credential would put it in a durable row that trajectory
    // observation and training exports both read.
    let (store, db_path) = temp_store("trajectory-secrets").await;
    let state = build_turso_state("trajectory-capture-secrets", store);

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders",
        serde_json::json!({"id": "ord-jcs-6", "Currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-6')/Temper.AddItem",
        serde_json::json!({
            "ProductId": "00000000-0000-0000-0000-000000000006",
            "Quantity": 1,
            "api_token": "sk-live-must-not-be-stored",
            "payment": {"card_number": "4111111111111111", "cvv": "123"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "AddItem failed: {body:?}");

    let entry = await_trajectory(&state, "AddItem with secrets", |entry| {
        entry.action == "AddItem" && entry.entity_id == "ord-jcs-6" && entry.success
    })
    .await;

    let request_body = entry.request_body.as_ref().expect("request body persisted");
    let rendered = request_body.to_string();
    assert!(
        !rendered.contains("sk-live-must-not-be-stored"),
        "the token must not survive into storage: {rendered}"
    );
    assert!(
        !rendered.contains("4111111111111111") && !rendered.contains("\"123\""),
        "nested payment details must not survive into storage: {rendered}"
    );
    assert_eq!(
        request_body["ProductId"],
        serde_json::json!("00000000-0000-0000-0000-000000000006"),
        "ordinary arguments are still captured"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn a_rejected_retry_records_the_state_it_was_attempted_from() {
    // A guard failure appends no event, so reading the newest event would
    // report the source state of the *previous* successful transition — the
    // state where the action was legal — and hide the illegal retry.
    let (store, db_path) = temp_store("trajectory-retry-source").await;
    let state = build_turso_state("trajectory-capture-retry-source", store);

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders",
        serde_json::json!({"id": "ord-jcs-7", "Currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-7')/Temper.AddItem",
        serde_json::json!({"ProductId": "00000000-0000-0000-0000-000000000007", "Quantity": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "AddItem failed: {body:?}");

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-7')/Temper.SubmitOrder",
        serde_json::json!({"ShippingAddressId": "10000000-0000-0000-0000-000000000001", "PaymentMethod": "card"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "SubmitOrder failed: {body:?}");

    // Second SubmitOrder: the order is already Submitted, where the action is
    // illegal, so the guard rejects it.
    let (status, _body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-7')/Temper.SubmitOrder",
        serde_json::json!({"ShippingAddressId": "10000000-0000-0000-0000-000000000001", "PaymentMethod": "card"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "SubmitOrder from Submitted must be rejected"
    );

    let entry = await_trajectory(&state, "rejected SubmitOrder retry", |entry| {
        entry.action == "SubmitOrder" && entry.entity_id == "ord-jcs-7" && !entry.success
    })
    .await;

    assert_eq!(
        entry.from_status.as_deref(),
        Some("Submitted"),
        "the rejected attempt was made from Submitted, not from the Draft the \
         previous successful transition started in"
    );
    assert_eq!(entry.to_status.as_deref(), Some("Submitted"));

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn a_session_reads_back_in_capture_order() {
    // Rows are persisted by independently spawned tasks, so the storage id is
    // the order the writes landed. The session read must reproduce the order
    // the kernel captured, which is what the conformance walk replays.
    let (store, db_path) = temp_store("trajectory-capture-order").await;
    let state = build_turso_state("trajectory-capture-order", store.clone());

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders",
        serde_json::json!({"id": "ord-jcs-8", "Currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    for quantity in 1..=6 {
        let (status, body) = post_observed(
            &state,
            "/tdata/Orders('ord-jcs-8')/Temper.AddItem",
            serde_json::json!({"ProductId": format!("00000000-0000-0000-0000-{quantity:012}"), "Quantity": quantity}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "AddItem failed: {body:?}");
    }
    let (status, body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-8')/Temper.SubmitOrder",
        serde_json::json!({"ShippingAddressId": "10000000-0000-0000-0000-000000000001", "PaymentMethod": "card"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "SubmitOrder failed: {body:?}");

    // Wait for the last captured row to land, then read the session back.
    let _ = await_trajectory(&state, "SubmitOrder", |entry| {
        entry.action == "SubmitOrder" && entry.entity_id == "ord-jcs-8" && entry.success
    })
    .await;

    let rows = store
        .query_trajectories_by_session(SESSION_ID, Some("default"), Some("Order"), 100)
        .await
        .expect("read session");
    let sequences: Vec<i64> = rows.iter().filter_map(|row| row.capture_seq).collect();
    assert_eq!(
        sequences.len(),
        rows.len(),
        "every captured row carries its capture order"
    );
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "the session must read back in capture order, got {sequences:?}"
    );
    let actions: Vec<&str> = rows.iter().map(|row| row.action.as_str()).collect();
    assert_eq!(
        actions.last(),
        Some(&"SubmitOrder"),
        "the last captured action is last in the read: {actions:?}"
    );

    let _ = std::fs::remove_file(db_path);
}
