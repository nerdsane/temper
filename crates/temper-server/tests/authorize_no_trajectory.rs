//! Offer checks must never enter the trajectory outbox, even for a large page.
//! A separate test binary installs its meter before the outbox initializes.
#![cfg(feature = "observe")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityVerificationResult, SpecRegistry, VerificationStatus};
use temper_server::{ServerState, StorageStack, build_router};
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const TENANT: &str = "default";

fn request(path: &str, body: serde_json::Value) -> Request<Body> {
    let mut request = Request::post(path)
        .header("content-type", "application/json")
        .header("x-session-id", "session-1")
        .body(Body::from(body.to_string()))
        .unwrap();
    request.extensions_mut().insert(
        AuthenticatedRequestContext::new(
            TenantId::new(TENANT),
            SecurityContext {
                principal: Principal {
                    id: "agent-1".to_string(),
                    kind: PrincipalKind::Agent,
                    role: None,
                    acting_for: None,
                    agent_type: None,
                    label: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "factory-sized-offers".to_string(),
                cause: None,
                credential: None,
            },
        )
        .with_session_id(Some("session-1".to_string())),
    );
    request
}

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

/// Read the synchronous enqueue counter, before background persistence can race
/// an absence assertion. GovernanceDecision has its own tenant and is excluded.
fn enqueued(provider: &SdkMeterProvider, exporter: &InMemoryMetricExporter) -> u64 {
    provider.force_flush().unwrap();
    let exported = exporter.get_finished_metrics().unwrap();
    exported
        .last()
        .into_iter()
        .flat_map(|batch| &batch.scope_metrics)
        .flat_map(|scope| &scope.metrics)
        .filter(|metric| metric.name == "temper_trajectory_outbox_enqueued_total")
        .flat_map(|metric| {
            &metric
                .data
                .as_any()
                .downcast_ref::<Sum<u64>>()
                .unwrap()
                .data_points
        })
        .filter(|point| {
            point
                .attributes
                .iter()
                .any(|kv| kv.key.as_str() == "tenant" && kv.value.as_str() == TENANT)
        })
        .map(|point| point.value)
        .sum()
}

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

#[tokio::test]
async fn factory_sized_checks_enqueue_nothing_but_a_refused_move_records_one_attempt() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());

    let temp = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", temp.path().join("metadata.db").display()),
        None,
    )
    .await
    .unwrap();
    let mut registry = SpecRegistry::new();
    for (tenant, csdl, entity_type, ioa) in [
        (
            TENANT,
            include_str!("../../../test-fixtures/specs/model.csdl.xml"),
            "Order",
            include_str!("../../../test-fixtures/specs/order.ioa.toml"),
        ),
        (
            "temper-system",
            include_str!("../../temper-platform/src/specs/model.csdl.xml"),
            "GovernanceDecision",
            include_str!("../../temper-platform/src/specs/GovernanceDecision.ioa.toml"),
        ),
    ] {
        registry.register_tenant(
            tenant,
            temper_spec::csdl::parse_csdl(csdl).unwrap(),
            csdl.to_string(),
            &[(entity_type, ioa)],
        );
        registry.set_verification_status(
            &TenantId::new(tenant),
            entity_type,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-10-28T00:00:00Z".to_string(),
            }),
        );
    }
    let mut state = ServerState::from_registry(ActorSystem::new("offer-outbox-proof"), registry);
    state.data_dir = temp.path().join("data");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
        .get_or_create_tenant_entity(
            &TenantId::new(TENANT),
            "Order",
            "order-1",
            serde_json::json!({}),
        )
        .await
        .unwrap();

    // A Factory-sized offer page: 50 allowed moves and 50 denied moves,
    // including a real spec action we will then dispatch on this same record.
    let mut actions = (0..99).map(|i| format!("Move{i}")).collect::<Vec<_>>();
    actions.push("AddItem".to_string());
    let policy = (0..99)
        .filter(|i| i % 2 == 0)
        .map(|i| {
            format!("permit(principal is Agent, action == Action::\"Move{i}\", resource is Order);")
        })
        .chain(std::iter::once(
            "permit(principal is Customer, action == Action::\"AddItem\", resource is Order);"
                .to_string(),
        ))
        .collect::<Vec<_>>()
        .join("\n");
    state.authz.reload_tenant_policies(TENANT, &policy).unwrap();
    let app = build_router(state.clone());
    assert_eq!(enqueued(&provider, &exporter), 0);

    let response = app
        .clone()
        .oneshot(request(
            "/api/authorize",
            serde_json::json!({
                "agent_id": "agent-1",
                "records": [{
                    "resource_type": "Order", "resource_id": "order-1",
                    "actions": actions, "context": {"status": "Draft"}
                }]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let batch = json_body(response).await;
    assert_eq!(
        enqueued(&provider, &exporter),
        0,
        "a batched offer check must enqueue no trajectory rows"
    );
    assert_eq!(filed(&state, &store).await, (0, 0, 0));
    let offers = batch["records"][0]["actions"].as_array().unwrap();
    assert_eq!(offers.len(), 100);

    for (i, offer) in offers.iter().enumerate() {
        let response = app
            .clone()
            .oneshot(request(
                "/api/authorize",
                serde_json::json!({
                    "agent_id": "agent-1", "resource_type": "Order", "resource_id": "order-1",
                    "action": actions[i], "context": {"status": "Draft"}
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let single = json_body(response).await;
        assert_eq!(offer["action"], actions[i]);
        assert_eq!(offer["allowed"], i < 99 && i % 2 == 0);
        assert_eq!(single["allowed"], offer["allowed"]);
        assert_eq!(single["reason"], offer["reason"]);
        if offer["allowed"] == false {
            assert!(
                offer["reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.is_empty())
            );
        }
    }
    assert_eq!(
        enqueued(&provider, &exporter),
        0,
        "single offer checks must also enqueue no trajectory rows"
    );
    assert_eq!(filed(&state, &store).await, (0, 0, 0));
    assert!(state.load_trajectory_entries(TENANT, 200).await.is_empty());

    let response = app
        .oneshot(request(
            "/tdata/Orders('order-1')/Temper.AddItem",
            serde_json::json!({"ProductId": "p1", "Quantity": 1}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let refusal = json_body(response).await;
    assert_eq!(enqueued(&provider, &exporter), 1, "one real refused move");
    assert_eq!(filed(&state, &store).await, (1, 1, 1));
    let decisions = store.query_all_decisions(None).await.unwrap();
    let decision: serde_json::Value = serde_json::from_str(&decisions[0]).unwrap();
    assert_eq!(refusal["decision_id"], decision["id"]);
    let governance_ids =
        state.list_entity_ids(&TenantId::new("temper-system"), "GovernanceDecision");
    assert_eq!(decision["governance_decision_id"], governance_ids[0]);

    let rows = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let rows = state.load_trajectory_entries(TENANT, 200).await;
            if !rows.is_empty() {
                break rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the refused move's trajectory must persist");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entity_type, "Order");
    assert_eq!(rows[0].entity_id, "order-1");
    assert_eq!(rows[0].action, "AddItem");
    assert_eq!(rows[0].agent_id.as_deref(), Some("agent-1"));
    assert_eq!(rows[0].session_id.as_deref(), Some("session-1"));
    assert_eq!(rows[0].authz_denied, Some(true));
    // OData's denial guard leaves this unset; conformance walks these rows.
    assert_eq!(rows[0].spec_governed, None);
    assert!(!rows[0].success);
}
