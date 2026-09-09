use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_store_turso::TursoEventStore;
use tokio_stream::StreamExt;
use tower::ServiceExt;

use crate::observe::build_observe_router;
use crate::registry::SpecRegistry;
use crate::state::{DecisionStatus, PendingDecision, ServerState};
use crate::storage::StorageStack;

async fn state_and_store() -> (ServerState, TursoEventStore) {
    let url = format!(
        "file:{}/temper-evolution-tenant-{}.db",
        std::env::temp_dir().display(),
        uuid::Uuid::new_v4()
    );
    let store = TursoEventStore::new(&url, None).await.unwrap();
    let mut state = ServerState::from_registry(
        ActorSystem::new("evolution-tenant-test"),
        SpecRegistry::new(),
    );
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
        .authz
        .reload_tenant_policies(
            "tenant-a",
            r#"permit(principal == Admin::"admin", action, resource);"#,
        )
        .expect("install explicit tenant-a test authority");
    (state, store)
}

fn admin_request(tenant: &str, request: Request<Body>) -> Request<Body> {
    let context = SecurityContext {
        principal: Principal {
            id: "admin".to_string(),
            kind: PrincipalKind::Admin,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "evolution-tenant-test".to_string(),
    };
    let mut request = request;
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            context,
        ));
    request
}

fn app(state: ServerState) -> Router {
    Router::new()
        .nest("/observe", build_observe_router())
        .with_state(state)
}

async fn response_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn record_and_feature_handlers_cannot_cross_tenants() {
    let (state, store) = state_and_store().await;
    for (tenant, id) in [("tenant-a", "O-a"), ("tenant-b", "O-b")] {
        store
            .insert_evolution_record(temper_store_turso::TursoEvolutionRecordInsert {
                tenant,
                id,
                record_type: "Observation",
                status: "Open",
                created_by: "test",
                derived_from: None,
                data_json: "{}",
            })
            .await
            .unwrap();
    }
    for (tenant, id) in [("tenant-a", "feature-a"), ("tenant-b", "feature-b")] {
        store
            .upsert_feature_request(tenant, id, "Workflow", id, 1, "[]", "Open", None)
            .await
            .unwrap();
    }
    let router = app(state);

    let response = router
        .clone()
        .oneshot(admin_request(
            "tenant-a",
            Request::get("/observe/evolution/records")
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    assert_eq!(json["records"].as_array().unwrap().len(), 1);
    assert_eq!(json["records"][0]["id"], "O-a");

    let response = router
        .clone()
        .oneshot(admin_request(
            "tenant-a",
            Request::get("/observe/evolution/records/O-b")
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = router
        .oneshot(admin_request(
            "tenant-a",
            Request::patch("/observe/evolution/feature-requests/feature-b")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"disposition":"Resolved"}"#))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let tenant_b = store.list_feature_requests("tenant-b", None).await.unwrap();
    assert_eq!(tenant_b[0].disposition, "Open");
}

fn pending_decision(tenant: &str, id: &str) -> PendingDecision {
    PendingDecision {
        id: id.to_string(),
        tenant: tenant.to_string(),
        agent_id: "agent".to_string(),
        action: "read".to_string(),
        resource_type: "Order".to_string(),
        resource_id: "order-1".to_string(),
        resource_attrs: serde_json::json!({}),
        denial_reason: "test".to_string(),
        module_name: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        status: DecisionStatus::Pending,
        decided_by: None,
        decided_at: None,
        generated_policy: None,
        approved_scope: None,
        evolution_record_id: None,
        agent_type: None,
        principal_kind: None,
        session_id: None,
        governance_decision_id: None,
    }
}

#[tokio::test]
async fn evolution_stream_emits_only_authenticated_tenant_events() {
    let (state, _) = state_and_store().await;
    let sender = state.pending_decision_tx.clone();
    let response = app(state)
        .oneshot(admin_request(
            "tenant-a",
            Request::get("/observe/evolution/stream")
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    sender
        .send(pending_decision("tenant-b", "decision-b"))
        .unwrap();
    sender
        .send(pending_decision("tenant-a", "decision-a"))
        .unwrap();

    let mut stream = response.into_body().into_data_stream();
    let mut text = String::new();
    while !text.contains("decision-a") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .expect("SSE frame timeout")
            .expect("SSE stream ended")
            .expect("SSE body error");
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(!text.contains("decision-b"));
}
