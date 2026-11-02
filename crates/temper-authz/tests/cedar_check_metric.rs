//! The Cedar evaluation counter says why Cedar was asked. A list read's per-row check is `check:row_filter` and every
//! other check is `check:gate`, so an alert on refusals can leave out the rows a policy hides on purpose: a private
//! app's list reads deny most rows for every caller but its owner, and that is the policy working.
//!
//! Its own test binary, so the global meter provider is set before anything in this process records a metric.

use std::collections::HashMap;

use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use temper_authz::{AuthzEngine, Principal, PrincipalKind, SecurityContext};

fn customer(id: &str) -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: id.to_string(),
            kind: PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: HashMap::new(),
        },
        context_attrs: HashMap::new(),
        correlation_id: "cedar-check-metric".to_string(),
        cause: None,
        credential: None,
    }
}

fn order(id: &str) -> HashMap<String, serde_json::Value> {
    HashMap::from([("id".to_string(), serde_json::Value::String(id.to_string()))])
}

fn value_of(attributes: &[KeyValue], key: &str) -> String {
    attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().to_string())
        .unwrap_or_default()
}

#[test]
fn a_list_rows_refusal_is_counted_as_a_row_filter_and_a_refused_read_as_a_gate() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());

    let engine = AuthzEngine::new("").expect("an empty fallback policy set");
    engine
        .reload_tenant_policies(
            "t1",
            r#"permit(principal, action == Action::"read", resource) when { resource.id == "mine" };"#,
        )
        .expect("the tenant's policy");
    let caller = customer("cust-1");

    let row = engine.authorize_row_for_tenant("t1", &caller, "read", "Order", &order("theirs"));
    let gate = engine.authorize_for_tenant("t1", &caller, "read", "Order", &order("theirs"));
    let allowed = engine.authorize_for_tenant("t1", &caller, "read", "Order", &order("mine"));
    assert!(
        !row.is_allowed() && !gate.is_allowed(),
        "both checks refuse a row that is not the caller's"
    );
    assert!(allowed.is_allowed());
    assert!(
        engine
            .authorize_row_for_tenant("t1", &caller, "read", "Order", &order("mine"))
            .is_allowed(),
        "the row check decides exactly as the gate does"
    );

    provider.force_flush().expect("flush metrics");
    let mut counted: HashMap<(String, String), u64> = HashMap::new();
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
                    *counted.entry(key).or_default() = point.value;
                }
            }
        }
    }
    let count = |check: &str, decision: &str| {
        counted
            .get(&(check.to_string(), decision.to_string()))
            .copied()
            .unwrap_or(0)
    };
    assert_eq!(
        count("row_filter", "deny"),
        1,
        "the list row left out: {counted:?}"
    );
    assert_eq!(count("row_filter", "allow"), 1, "{counted:?}");
    assert_eq!(
        count("gate", "deny"),
        1,
        "the refused single read: {counted:?}"
    );
    assert_eq!(count("gate", "allow"), 1, "{counted:?}");
}
