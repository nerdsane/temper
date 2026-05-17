//! Trace sampler rules and metrics for OTEL setup.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge};
use opentelemetry::trace::{Link, SamplingDecision, SamplingResult, SpanKind, TraceId, TraceState};
use opentelemetry_sdk::trace::{Sampler, ShouldSample};

use super::config::read_non_empty_env;

/// Span names dropped at the sampler layer: trace volume noise with no
/// operator value. See ADR-0052/0053 (instrumentation policy and hygiene).
///
/// - `turso.configured_connection`: DB pool internal; no timing value.
/// - `clock_time_get`: WASI guest syscall; fires per-LLM-token in high volume.
const DROPPED_SPAN_NAMES: &[&str] = &["turso.configured_connection", "clock_time_get"];

const WASM_AUXILIARY_SAMPLE_RATE_ENV: &str = "TEMPER_TRACE_WASM_AUX_SAMPLE_PCT";
const DISPATCH_BACKGROUND_SAMPLE_RATE_ENV: &str = "TEMPER_TRACE_DISPATCH_BACKGROUND_SAMPLE_PCT";
pub(super) const WASM_AUXILIARY_SAMPLE_RATE_DEFAULT: u8 = 5;
pub(super) const DISPATCH_BACKGROUND_SAMPLE_RATE_DEFAULT: u8 = 25;
/// Span name prefixes that are sampled at a reduced rate. Do not include
/// tool/provider guest module boundaries or their background dispatch parents
/// here: guest spans inherit parent sampling, so reducing those boundaries can
/// sever traces operators need when following work through WASM.
pub(super) const WASM_AUXILIARY_PREFIXES: &[&str] = &["wasm:workspace_fs"];
pub(super) const DISPATCH_BACKGROUND_PREFIXES: &[&str] = &[
    "dispatch.phase.query_projection",
    "dispatch.scheduled_actions",
];

#[derive(Clone, Debug)]
struct ReducedSampleRule {
    name: &'static str,
    prefixes: &'static [&'static str],
    sample_rate_pct: u8,
}

impl ReducedSampleRule {
    fn matches(&self, span_name: &str) -> bool {
        self.prefixes
            .iter()
            .any(|prefix| span_name.starts_with(prefix))
    }
}

#[derive(Clone, Debug)]
pub(super) struct TraceSamplerConfig {
    reduced_rules: Vec<ReducedSampleRule>,
}

impl Default for TraceSamplerConfig {
    fn default() -> Self {
        Self::with_rates(
            WASM_AUXILIARY_SAMPLE_RATE_DEFAULT,
            DISPATCH_BACKGROUND_SAMPLE_RATE_DEFAULT,
        )
    }
}

impl TraceSamplerConfig {
    pub(super) fn from_env() -> Self {
        Self::with_rates(
            sample_rate_from_env(
                WASM_AUXILIARY_SAMPLE_RATE_ENV,
                WASM_AUXILIARY_SAMPLE_RATE_DEFAULT,
            ),
            sample_rate_from_env(
                DISPATCH_BACKGROUND_SAMPLE_RATE_ENV,
                DISPATCH_BACKGROUND_SAMPLE_RATE_DEFAULT,
            ),
        )
    }

    fn with_rates(
        wasm_auxiliary_sample_rate_pct: u8,
        dispatch_background_sample_rate_pct: u8,
    ) -> Self {
        Self {
            reduced_rules: vec![
                ReducedSampleRule {
                    name: "wasm_auxiliary",
                    prefixes: WASM_AUXILIARY_PREFIXES,
                    sample_rate_pct: wasm_auxiliary_sample_rate_pct,
                },
                ReducedSampleRule {
                    name: "dispatch_background",
                    prefixes: DISPATCH_BACKGROUND_PREFIXES,
                    sample_rate_pct: dispatch_background_sample_rate_pct,
                },
            ],
        }
    }

    pub(super) fn reduced_rule_rate(&self, rule_name: &str) -> Option<u8> {
        self.reduced_rules
            .iter()
            .find(|rule| rule.name == rule_name)
            .map(|rule| rule.sample_rate_pct)
    }

    pub(super) fn reduced_prefix_count(&self) -> usize {
        self.reduced_rules
            .iter()
            .map(|rule| rule.prefixes.len())
            .sum()
    }
}

struct TraceSamplerMetrics {
    decisions_total: Counter<u64>,
    configured_rules: Gauge<u64>,
    reduced_sample_rate_pct: Gauge<u64>,
}

fn trace_sampler_metrics() -> &'static TraceSamplerMetrics {
    static METRICS: OnceLock<TraceSamplerMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = opentelemetry::global::meter("temper.observe");
        TraceSamplerMetrics {
            decisions_total: meter
                .u64_counter("temper_trace_sampler_decisions_total")
                .with_description(
                    "Trace sampler decisions by bounded rule name and final sampling decision.",
                )
                .build(),
            configured_rules: meter
                .u64_gauge("temper_trace_sampler_configured_rules")
                .with_description(
                    "Configured trace sampler rules by rule kind. Values are counts, not rates.",
                )
                .build(),
            reduced_sample_rate_pct: meter
                .u64_gauge("temper_trace_sampler_reduced_sample_rate_pct")
                .with_unit("%")
                .with_description("Effective trace sampler keep rate for reduced-prefix rules.")
                .build(),
        }
    })
}

fn sample_rate_from_env(var: &str, default: u8) -> u8 {
    read_non_empty_env(var)
        .and_then(|value| value.parse::<u16>().ok())
        .map(|value| value.min(100) as u8)
        .unwrap_or(default)
}

fn sampling_decision_label(decision: &SamplingDecision) -> &'static str {
    match decision {
        SamplingDecision::Drop => "drop",
        SamplingDecision::RecordOnly => "record_only",
        SamplingDecision::RecordAndSample => "record_and_sample",
    }
}

fn record_trace_sampler_decision(rule: &'static str, decision: &'static str) {
    trace_sampler_metrics().decisions_total.add(
        1,
        &[
            KeyValue::new("rule", rule),
            KeyValue::new("decision", decision),
        ],
    );
}

pub(super) fn record_trace_sampler_config(config: &TraceSamplerConfig) {
    trace_sampler_metrics().configured_rules.record(
        DROPPED_SPAN_NAMES.len() as u64,
        &[KeyValue::new("rule_kind", "drop_exact")],
    );
    trace_sampler_metrics().configured_rules.record(
        config.reduced_prefix_count() as u64,
        &[KeyValue::new("rule_kind", "reduced_prefix")],
    );
    for rule in &config.reduced_rules {
        trace_sampler_metrics().reduced_sample_rate_pct.record(
            rule.sample_rate_pct as u64,
            &[KeyValue::new("rule", rule.name)],
        );
    }
}

/// Trace-id-based deterministic sampling: keep if `(trace_id_low_u64 % 100) < rate_pct`.
/// Using trace_id instead of random keeps the decision consistent across
/// span siblings in the same trace, so a sampled trace keeps all its spans.
fn trace_id_sample_at(trace_id: TraceId, rate_pct: u8) -> bool {
    let bytes = trace_id.to_bytes();
    let mut low = 0u64;
    for b in &bytes[8..16] {
        low = (low << 8) | (*b as u64);
    }
    (low % 100) < rate_pct as u64
}

/// Sampler that drops specific span names outright, reduced-samples specific
/// prefixes, and delegates every other span decision to the wrapped inner
/// sampler (default: parent-based AlwaysOn).
///
/// Implemented here rather than configured via `OTEL_TRACES_SAMPLER_ARG` so
/// the drop rules live in source (grep-able) and survive env-var rewrites by
/// tenant/deploy tooling.
#[derive(Debug, Clone)]
pub(super) struct NameBasedSampler {
    pub(super) inner: Sampler,
    pub(super) config: TraceSamplerConfig,
}

impl ShouldSample for NameBasedSampler {
    fn should_sample(
        &self,
        parent_context: Option<&opentelemetry::Context>,
        trace_id: TraceId,
        name: &str,
        span_kind: &SpanKind,
        attributes: &[KeyValue],
        links: &[Link],
    ) -> SamplingResult {
        if DROPPED_SPAN_NAMES.contains(&name) {
            record_trace_sampler_decision("drop_exact", "drop");
            return SamplingResult {
                decision: SamplingDecision::Drop,
                attributes: Vec::new(),
                trace_state: TraceState::default(),
            };
        }
        if let Some(rule) = self
            .config
            .reduced_rules
            .iter()
            .find(|rule| rule.matches(name))
        {
            if !trace_id_sample_at(trace_id, rule.sample_rate_pct) {
                record_trace_sampler_decision(rule.name, "reduced_drop");
                return SamplingResult {
                    decision: SamplingDecision::Drop,
                    attributes: Vec::new(),
                    trace_state: TraceState::default(),
                };
            }
            let result = self.inner.should_sample(
                parent_context,
                trace_id,
                name,
                span_kind,
                attributes,
                links,
            );
            record_trace_sampler_decision(rule.name, sampling_decision_label(&result.decision));
            return result;
        }

        let result =
            self.inner
                .should_sample(parent_context, trace_id, name, span_kind, attributes, links);
        record_trace_sampler_decision("delegate", sampling_decision_label(&result.decision));
        result
    }
}
