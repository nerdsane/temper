use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceAppend, PersistenceEnvelope,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn state_with_store(store: SimEventStore, name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).unwrap(),
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA), ("Payment", ORDER_IOA)],
    );
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
            verified_at: "2026-07-10T00:00:00Z".to_string(),
        }),
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.single_tenant_mode = true;
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
}

fn created_payment(persistence_id: &str, order_id: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Created".to_string(),
        payload: serde_json::json!({
            "action": "Created",
            "from_status": "",
            "to_status": "Draft",
            "timestamp": sim_now(),
            "params": {"OrderId": order_id},
            "idempotency_key": null
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: persistence_id.to_string(),
        },
    }
}

async fn delete(state: &ServerState, order_id: &str) -> (StatusCode, serde_json::Value) {
    let response = build_router(state.clone())
        .oneshot(
            Request::delete(format!("/tdata/Orders('{order_id}')"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn delete_fails_closed_when_referencing_source_replay_is_uncertain() {
    let store = SimEventStore::no_faults(192_950);
    let state = state_with_store(store.clone(), "delete-relation-replay-failure");
    let tenant = TenantId::default();
    let order_id = "ord-relation-target";
    let payment_id = "pay-corrupt-reference";
    let payment_pid = format!("{tenant}:Payment:{payment_id}");
    state
        .get_or_create_tenant_entity(&tenant, "Order", order_id, serde_json::json!({}))
        .await
        .unwrap();
    store
        .append(
            &payment_pid,
            0,
            &[
                created_payment(&payment_pid, order_id),
                PersistenceEnvelope {
                    sequence_nr: 0,
                    event_type: "PaymentSchemaV2".to_string(),
                    payload: serde_json::json!({"backend_detail": "secret-driver-diagnostic"}),
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: sim_now(),
                        actor_id: payment_pid.clone(),
                    },
                },
            ],
        )
        .await
        .unwrap();

    let (status, body) = delete(&state, order_id).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "RelationCheckUnavailable");
    assert!(!body.to_string().contains("secret-driver-diagnostic"));
    assert!(
        !temper_runtime::persistence::is_deletion_tombstone(
            store
                .read_latest_events(&[format!("{tenant}:Order:{order_id}")])
                .await
                .unwrap()[0]
                .as_ref()
                .unwrap()
        ),
        "uncertain Restrict scan must not commit the target deletion"
    );
}

#[tokio::test]
async fn delete_relation_scan_has_an_explicit_candidate_budget() {
    let store = SimEventStore::no_faults(192_951);
    let state = state_with_store(store.clone(), "delete-relation-budget");
    let tenant = TenantId::default();
    let order_id = "ord-relation-budget";
    state
        .get_or_create_tenant_entity(&tenant, "Order", order_id, serde_json::json!({}))
        .await
        .unwrap();

    let appends = (0..513)
        .map(|index| {
            let payment_id = format!("pay-budget-{index:04}");
            let persistence_id = format!("{tenant}:Payment:{payment_id}");
            PersistenceAppend {
                persistence_id: persistence_id.clone(),
                expected_sequence: 0,
                events: vec![created_payment(&persistence_id, "different-order")],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            }
        })
        .collect::<Vec<_>>();
    store.append_batch(&appends).await.unwrap();

    let (status, body) = delete(&state, order_id).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "RelationCheckUnavailable");
}
