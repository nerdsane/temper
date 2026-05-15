use super::config::{LOGFIRE_ENDPOINT, resolve_otel_config};
use super::sampler::{DISPATCH_BACKGROUND_PREFIXES, WASM_AUXILIARY_PREFIXES};
use super::*;

use opentelemetry::trace::{SamplingDecision, SpanKind, TraceId};
use opentelemetry_sdk::trace::ShouldSample;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());
const TEST_ENV_VARS: [&str; 5] = [
    "OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "LOGFIRE_TOKEN",
    "TEMPER_TRACE_WASM_AUX_SAMPLE_PCT",
    "TEMPER_TRACE_DISPATCH_BACKGROUND_SAMPLE_PCT",
];

fn with_test_env(values: &[(&str, Option<&str>)], f: impl FnOnce()) {
    let _guard = ENV_LOCK.lock().expect("env mutex must lock");
    let snapshot: Vec<(&str, Option<String>)> = TEST_ENV_VARS
        .iter()
        .map(|key| (*key, std::env::var(key).ok()))
        .collect();

    for (key, value) in values {
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    f();

    for (key, value) in snapshot {
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}

#[test]
fn resolve_config_prefers_otlp_endpoint() {
    with_test_env(
        &[
            ("OTLP_ENDPOINT", Some("http://otlp:4318")),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://other:4318")),
            ("LOGFIRE_TOKEN", Some("abc123")),
        ],
        || {
            let config = resolve_otel_config().expect("config should resolve");
            assert_eq!(config.endpoint, "http://otlp:4318");
            assert_eq!(config.endpoint_source.as_str(), "OTLP_ENDPOINT");
            assert_eq!(config.logfire_token.as_deref(), Some("abc123"));
        },
    );
}

#[test]
fn resolve_config_uses_exporter_endpoint_when_otlp_missing() {
    with_test_env(
        &[
            ("OTLP_ENDPOINT", None),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://collector:4318")),
            ("LOGFIRE_TOKEN", None),
        ],
        || {
            let config = resolve_otel_config().expect("config should resolve");
            assert_eq!(config.endpoint, "http://collector:4318");
            assert_eq!(
                config.endpoint_source.as_str(),
                "OTEL_EXPORTER_OTLP_ENDPOINT"
            );
            assert_eq!(config.logfire_token, None);
        },
    );
}

#[test]
fn resolve_config_falls_back_to_logfire_token() {
    with_test_env(
        &[
            ("OTLP_ENDPOINT", None),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", None),
            ("LOGFIRE_TOKEN", Some("abc123")),
        ],
        || {
            let config = resolve_otel_config().expect("config should resolve");
            assert_eq!(config.endpoint, LOGFIRE_ENDPOINT);
            assert_eq!(config.endpoint_source.as_str(), "LOGFIRE_TOKEN");
            assert_eq!(config.logfire_token.as_deref(), Some("abc123"));
        },
    );
}

#[test]
fn name_based_sampler_drops_configured_span_names() {
    let sampler = NameBasedSampler {
        inner: Sampler::AlwaysOn,
        config: TraceSamplerConfig::default(),
    };
    let trace_id = TraceId::from_bytes([0u8; 16]);

    for dropped in ["turso.configured_connection", "clock_time_get"] {
        let result = sampler.should_sample(None, trace_id, dropped, &SpanKind::Internal, &[], &[]);
        assert!(
            matches!(result.decision, SamplingDecision::Drop),
            "sampler must drop {dropped}",
        );
    }
}

#[test]
fn reduced_sample_prefixes_keep_roughly_5_percent() {
    let sampler = NameBasedSampler {
        inner: Sampler::AlwaysOn,
        config: TraceSamplerConfig::default(),
    };
    let mut kept = 0usize;
    let total = 10_000usize;
    for i in 0..total {
        let mut bytes = [0u8; 16];
        bytes[8..].copy_from_slice(&(i as u64).to_be_bytes());
        let trace_id = TraceId::from_bytes(bytes);
        let r = sampler.should_sample(
            None,
            trace_id,
            "wasm:workspace_fs.read",
            &SpanKind::Internal,
            &[],
            &[],
        );
        if matches!(r.decision, SamplingDecision::RecordAndSample) {
            kept += 1;
        }
    }
    let kept_pct = (kept as f64 / total as f64) * 100.0;
    assert!(
        (3.0..=7.0).contains(&kept_pct),
        "reduced sample should keep ~5%, got {kept_pct:.1}%"
    );
}

#[test]
fn monty_repl_boundary_is_not_reduced_sampled() {
    let sampler = NameBasedSampler {
        inner: Sampler::AlwaysOn,
        config: TraceSamplerConfig::default(),
    };
    let trace_id = TraceId::from_bytes([0u8; 16]);
    let result = sampler.should_sample(
        None,
        trace_id,
        "wasm:monty_repl",
        &SpanKind::Internal,
        &[],
        &[],
    );

    assert!(
        matches!(result.decision, SamplingDecision::RecordAndSample),
        "monty_repl must keep full sampling so guest tool spans stay stitched"
    );
}

#[test]
fn name_based_sampler_delegates_other_spans() {
    let sampler = NameBasedSampler {
        inner: Sampler::AlwaysOn,
        config: TraceSamplerConfig::default(),
    };
    let trace_id = TraceId::from_bytes([0u8; 16]);
    let result = sampler.should_sample(
        None,
        trace_id,
        "dispatch.dispatch_tenant_action_core",
        &SpanKind::Internal,
        &[],
        &[],
    );
    assert!(
        matches!(result.decision, SamplingDecision::RecordAndSample),
        "non-dropped span must keep AlwaysOn decision",
    );
}

#[test]
fn resolve_config_ignores_empty_values() {
    with_test_env(
        &[
            ("OTLP_ENDPOINT", Some("   ")),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("")),
            ("LOGFIRE_TOKEN", Some(" ")),
        ],
        || {
            let config = resolve_otel_config();
            assert!(config.is_none(), "all-empty env vars should disable OTEL");
        },
    );
}

#[test]
fn trace_sampler_config_reads_and_clamps_sample_rate_overrides() {
    with_test_env(
        &[
            ("TEMPER_TRACE_WASM_AUX_SAMPLE_PCT", Some("17")),
            ("TEMPER_TRACE_DISPATCH_BACKGROUND_SAMPLE_PCT", Some("250")),
        ],
        || {
            let config = TraceSamplerConfig::from_env();
            assert_eq!(config.reduced_rule_rate("wasm_auxiliary"), Some(17));
            assert_eq!(config.reduced_rule_rate("dispatch_background"), Some(100));
            assert_eq!(
                config.reduced_prefix_count(),
                WASM_AUXILIARY_PREFIXES.len() + DISPATCH_BACKGROUND_PREFIXES.len()
            );
        },
    );
}
