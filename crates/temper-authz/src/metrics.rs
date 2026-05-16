use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::{KeyValue, global};

struct CedarMetrics {
    evaluations_total: Counter<u64>,
    evaluation_duration: Histogram<f64>,
    evaluation_duration_ms: Histogram<f64>,
    evaluation_phase_duration_ms: Histogram<f64>,
    policy_candidate_count: Histogram<u64>,
    request_attribute_count: Histogram<u64>,
}

fn metrics() -> &'static CedarMetrics {
    static METRICS: OnceLock<CedarMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.authz");
        CedarMetrics {
            evaluations_total: meter
                .u64_counter("temper_cedar_evaluations_total")
                .with_description("Total number of Cedar authorization evaluations.")
                .build(),
            evaluation_duration: meter
                .f64_histogram("temper_cedar_evaluation_duration")
                .with_description("Latency of Cedar authorization evaluation.")
                .build(),
            evaluation_duration_ms: meter
                .f64_histogram("temper_cedar_evaluation_duration_ms")
                .with_description("Latency of Cedar authorization evaluation in milliseconds.")
                .with_unit("ms")
                .build(),
            evaluation_phase_duration_ms: meter
                .f64_histogram("temper_cedar_evaluation_phase_duration_ms")
                .with_description(
                    "Latency of bounded phases inside Cedar authorization evaluation.",
                )
                .with_unit("ms")
                .build(),
            policy_candidate_count: meter
                .u64_histogram("temper_cedar_policy_candidate_count")
                .with_description(
                    "Full and candidate Cedar policy counts considered for authorization.",
                )
                .build(),
            request_attribute_count: meter
                .u64_histogram("temper_cedar_request_attribute_count")
                .with_description("Attribute counts in Cedar authorization request inputs.")
                .build(),
        }
    })
}

pub fn init_metrics() {
    let _ = metrics();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CedarDecisionMetric {
    Allow,
    Deny,
    Error,
}

impl CedarDecisionMetric {
    fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CedarPhaseOutcome {
    Ok,
    Error,
}

impl CedarPhaseOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

pub(crate) fn record_cedar_evaluation(duration: Duration, decision: CedarDecisionMetric) {
    let attrs = [KeyValue::new("decision", decision.as_str().to_string())];
    metrics().evaluations_total.add(1, &attrs);
    metrics()
        .evaluation_duration
        .record(duration.as_secs_f64(), &attrs);
    metrics()
        .evaluation_duration_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
}

pub(crate) fn record_cedar_phase_duration(
    phase: &'static str,
    duration: Duration,
    outcome: CedarPhaseOutcome,
) {
    let attrs = [
        KeyValue::new("phase", phase.to_string()),
        KeyValue::new("outcome", outcome.as_str().to_string()),
    ];
    metrics()
        .evaluation_phase_duration_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
}

pub(crate) fn record_cedar_request_attribute_count(source: &'static str, count: usize) {
    let attrs = [KeyValue::new("source", source.to_string())];
    metrics()
        .request_attribute_count
        .record(count as u64, &attrs);
}

pub(crate) fn record_cedar_policy_candidate_counts(
    full_count: usize,
    candidate_count: usize,
    outcome: &'static str,
) {
    let full_attrs = [
        KeyValue::new("source", "full".to_string()),
        KeyValue::new("outcome", outcome.to_string()),
    ];
    metrics()
        .policy_candidate_count
        .record(full_count as u64, &full_attrs);

    let candidate_attrs = [
        KeyValue::new("source", "candidate".to_string()),
        KeyValue::new("outcome", outcome.to_string()),
    ];
    metrics()
        .policy_candidate_count
        .record(candidate_count as u64, &candidate_attrs);
}
