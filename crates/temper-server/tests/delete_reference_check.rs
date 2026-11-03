//! A `Restrict` delete is answered from the query index, not by reading every
//! row that could reference the entity.
//!
//! The fixture CSDL declares `Payment.OrderId -> Order` with the default
//! `Restrict` delete policy. One server writes the rows; a second server opens
//! the same store, as a restarted process does, so no Payment is resident in
//! memory when an Order is deleted. Reading a Payment would start its actor, so
//! the server's live actor count shows whether the delete read them.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{CSDL_XML, ORDER_IOA};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

/// `Payment` only needs to exist and carry the `OrderId` it was created with.
const PAYMENT_IOA: &str = r#"
[automaton]
name = "Payment"
states = ["Pending"]
initial = "Pending"
"#;

/// Enough referencing rows that reading them shows up plainly in the actor count.
const REFERENCING_PAYMENTS: usize = 40;

struct Store {
    db_path: std::path::PathBuf,
    db_url: String,
}

impl Store {
    fn new(name: &str) -> Self {
        let db_path = std::env::temp_dir().join(format!(
            "temper-delete-reference-{name}-{}.db",
            uuid::Uuid::new_v4()
        ));
        let db_url = format!("file:{}", db_path.display());
        Self { db_path, db_url }
    }

    async fn open(&self) -> TursoEventStore {
        TursoEventStore::new(&self.db_url, None)
            .await
            .expect("open local turso db")
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
    }
}

fn server(name: &str, stack: StorageStack) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL parse"),
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA), ("Payment", PAYMENT_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(stack);
    {
        let mut registry = state.registry.write().unwrap();
        for entity_type in ["Order", "Payment"] {
            registry.set_verification_status(
                &TenantId::default(),
                entity_type,
                VerificationStatus::Completed(EntityVerificationResult {
                    all_passed: true,
                    levels: vec![EntityLevelSummary {
                        level: "L0 SMT".to_string(),
                        passed: true,
                        summary: "OK".to_string(),
                        details: None,
                    }],
                    verified_at: "2026-10-24T00:00:00Z".to_string(),
                }),
            );
        }
    }
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("test policy parses");
    state
}

/// A server whose writes reach the journal and the query projection.
async fn indexed_server(name: &str, store: &Store) -> ServerState {
    server(name, StorageStack::from_turso(store.open().await))
}

/// The same store opened by a fresh process: ids indexed at boot, no actor resident.
async fn restarted_server(name: &str, store: &Store) -> ServerState {
    let state = indexed_server(name, store).await;
    state.populate_index_from_store(&TenantId::default()).await;
    state
}

async fn create(state: &ServerState, entity_type: &str, id: &str, fields: serde_json::Value) {
    state
        .get_or_create_tenant_entity(&TenantId::default(), entity_type, id, fields)
        .await
        .unwrap_or_else(|error| panic!("create {entity_type}('{id}'): {error}"));
}

async fn create_payments(state: &ServerState, prefix: &str, count: usize, order_id: &str) {
    for n in 0..count {
        create(
            state,
            "Payment",
            &format!("{prefix}-{n}"),
            serde_json::json!({ "OrderId": order_id }),
        )
        .await;
    }
}

async fn send(state: &ServerState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let mut request = request;
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "delete-reference-test",
                "test-agent",
                None,
            ),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn delete_order(state: &ServerState, id: &str) -> (StatusCode, serde_json::Value) {
    let path = format!("/tdata/Orders('{id}')");
    send(state, Request::delete(path).body(Body::empty()).unwrap()).await
}

async fn get_order(state: &ServerState, id: &str) -> StatusCode {
    let path = format!("/tdata/Orders('{id}')");
    send(state, Request::get(path).body(Body::empty()).unwrap())
        .await
        .0
}

#[tokio::test]
async fn restrict_delete_is_answered_without_reading_the_referencing_rows() {
    let store = Store::new("indexed");
    let writer = indexed_server("delete-reference-writer", &store).await;
    create(&writer, "Order", "ord-referenced", serde_json::json!({})).await;
    create(&writer, "Order", "ord-unreferenced", serde_json::json!({})).await;
    create_payments(&writer, "pay", REFERENCING_PAYMENTS, "ord-referenced").await;

    let state = restarted_server("delete-reference-restarted", &store).await;
    let actors_before = state.active_actor_count();

    let (status, body) = delete_order(&state, "ord-unreferenced").await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "unreferenced delete: {body}"
    );

    let (status, body) = delete_order(&state, "ord-referenced").await;
    assert_eq!(status, StatusCode::CONFLICT, "referenced delete: {body}");
    assert_eq!(body["error"]["details"]["type"], "relation_integrity");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("referenced by Payment('pay-"),
        "refusal names a referencing Payment: {message}"
    );

    // Only the two Orders themselves were loaded; none of the Payments.
    let actors_started = state.active_actor_count().saturating_sub(actors_before);
    assert!(
        actors_started <= 2,
        "the deletes started {actors_started} actors; reading the \
         {REFERENCING_PAYMENTS} Payments would start one each"
    );
    assert_eq!(
        get_order(&state, "ord-unreferenced").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get_order(&state, "ord-referenced").await, StatusCode::OK);
}

#[tokio::test]
async fn restrict_delete_uses_the_stored_top_level_reference() {
    let store = Store::new("wrapped");
    let writer = indexed_server("delete-reference-wrapped-writer", &store).await;
    create(&writer, "Order", "ord-wrapped", serde_json::json!({})).await;
    create_payments(&writer, "pay", REFERENCING_PAYMENTS, "ord-elsewhere").await;
    create(&writer, "Order", "ord-top-level", serde_json::json!({})).await;
    // Relations read the stored record's top-level properties. A nested
    // `fields` object is arbitrary data, not another record envelope.
    create(
        &writer,
        "Payment",
        "pay-wrapped",
        serde_json::json!({
            "OrderId": "ord-top-level",
            "fields": { "OrderId": "ord-wrapped" }
        }),
    )
    .await;

    let state = restarted_server("delete-reference-wrapped-restarted", &store).await;
    let actors_before = state.active_actor_count();
    let (status, body) = delete_order(&state, "ord-wrapped").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "nested data: {body}");
    let (status, body) = delete_order(&state, "ord-top-level").await;
    assert_eq!(status, StatusCode::CONFLICT, "stored reference: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("referenced by Payment('pay-wrapped')"),
        "refusal names the Payment with the top-level reference: {body}"
    );
    let actors_started = state.active_actor_count().saturating_sub(actors_before);
    assert!(
        actors_started <= 2,
        "the deletes started {actors_started} actors; the reference is \
         read from its projection, not its actor"
    );
    assert_eq!(
        get_order(&state, "ord-wrapped").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get_order(&state, "ord-top-level").await, StatusCode::OK);
}
