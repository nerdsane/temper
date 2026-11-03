//! A page asking which moves to show (`POST /api/authorize`) is counted as `check:offer`, apart from the `check:gate` of
//! a request. A "no" there hides a button; it is not a refusal, so an alert on refusals, which reads `check:gate`,
//! leaves it out however many records a page asks about. A question writes nothing, so this count is its only record.
//!
//! Its own test binary, so the global meter provider is set before anything in this process records a metric.
#![cfg(feature = "observe")]

use std::collections::{BTreeMap, HashMap};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::{ServerState, build_router};
use tower::ServiceExt;

const TENANT: &str = "default";

fn customer(id: &str) -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: id.to_string(),
            kind: PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "authorize-offer-metric".to_string(),
        cause: None,
        credential: None,
    }
}

fn value_of(attributes: &[KeyValue], key: &str) -> String {
    attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().to_string())
        .unwrap_or_default()
}

/// `temper_cedar_evaluations_total` by `(check, decision)`.
fn cedar_counts(
    provider: &SdkMeterProvider,
    exporter: &InMemoryMetricExporter,
) -> HashMap<(String, String), u64> {
    provider.force_flush().expect("flush metrics");
    let mut counted = HashMap::new();
    for metrics in exporter.get_finished_metrics().expect("metrics") {
        for scope in &metrics.scope_metrics {
            for metric in scope
                .metrics
                .iter()
                .filter(|m| m.name == "temper_cedar_evaluations_total")
            {
                let sum = metric
                    .data
                    .as_any()
                    .downcast_ref::<Sum<u64>>()
                    .expect("a counter");
                for point in &sum.data_points {
                    let key = (
                        value_of(&point.attributes, "check"),
                        value_of(&point.attributes, "decision"),
                    );
                    counted.insert(key, point.value);
                }
            }
        }
    }
    counted
}

/// A page asks about three records, one the caller may read and two it may not, and the caller then sends a read of
/// one of the two. The page's three answers are offers; only the read is a gate check, and only it is a refusal.
#[tokio::test]
async fn a_pages_question_is_counted_as_an_offer_and_a_refused_request_as_a_gate() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());

    let state = ServerState::from_registry(
        ActorSystem::new("authorize-offer-metric"),
        SpecRegistry::new(),
    );
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"permit(principal, action == Action::"read", resource) when { resource.id == "mine" };"#,
        )
        .expect("the tenant's policy");
    let caller = customer("cust-1");

    let records = ["mine", "theirs-1", "theirs-2"].map(
        |id| serde_json::json!({"resource_type": "Order", "resource_id": id, "actions": ["read"]}),
    );
    let mut page = Request::builder()
        .method(Method::POST)
        .uri("/api/authorize")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"agent_id": "cust-1", "records": records}).to_string(),
        ))
        .expect("request should build");
    page.extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(TENANT),
            caller.clone(),
        ));
    let answered = build_router(state.clone()).oneshot(page).await.unwrap();
    assert_eq!(answered.status(), StatusCode::OK);
    let answered: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(answered.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let allowed = answered["records"]
        .as_array()
        .expect("one answer per record")
        .iter()
        .map(|record| record["actions"][0]["allowed"].as_bool())
        .collect::<Vec<_>>();
    assert_eq!(allowed, [Some(true), Some(false), Some(false)]);

    let read = state.authorize_with_context(
        &caller,
        "read",
        "Order",
        &BTreeMap::from([("id".to_string(), serde_json::json!("theirs-1"))]),
        TENANT,
    );
    assert!(
        read.is_err(),
        "the read of a record not the caller's is refused"
    );

    let counted = cedar_counts(&provider, &exporter);
    let count = |check: &str, decision: &str| {
        counted
            .get(&(check.to_string(), decision.to_string()))
            .copied()
            .unwrap_or(0)
    };
    assert_eq!(
        (count("offer", "allow"), count("offer", "deny")),
        (1, 2),
        "the page's three answers: {counted:?}"
    );
    assert_eq!(
        (count("gate", "allow"), count("gate", "deny")),
        (0, 1),
        "only the refused read: {counted:?}"
    );
}
