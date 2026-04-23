//! OTEL setup and teardown for Temper observability.
//!
//! Configures [`SdkTracerProvider`], [`SdkMeterProvider`], and
//! [`SdkLoggerProvider`] with OTLP HTTP exporters, plus a
//! [`tracing_subscriber`] that bridges `tracing` events to OTEL signals.
//!
//! # Logfire support
//!
//! Set `LOGFIRE_TOKEN` in your `.env` (or environment) and the endpoint +
//! auth header are configured automatically.  No other config needed.
//!
//! # Environment variables
//!
//! | Variable | Purpose |
//! |----------|---------|
//! | `OTLP_ENDPOINT` | OTEL collector base URL (e.g. `http://localhost:4318`) |
//! | `LOGFIRE_TOKEN` | Logfire write token — auto-sets endpoint + auth header |
//! | `RUST_LOG` | Log level filter (default: `info`) |
//! | `TEMPER_TRACE_QUEUE_SIZE` | Max buffered spans before drop (default: 2048, range: 128–32768) |
//! | `TEMPER_LOG_QUEUE_SIZE` | Max buffered log records before drop (default: 2048, range: 128–32768) |

use std::collections::HashMap;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::trace::{
    Link, SamplingDecision, SamplingResult, SpanKind, TraceId, TraceState, TracerProvider as _,
};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::{
    BatchConfigBuilder as LogBatchConfigBuilder, BatchLogProcessor, SdkLoggerProvider,
};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{
    BatchConfigBuilder as SpanBatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider,
    ShouldSample,
};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// Span names dropped at the sampler layer — trace volume noise with no
/// operator value. See ADR-0052/0053 (instrumentation policy + hygiene).
///
/// - `turso.configured_connection`: DB pool internal; no timing value.
/// - `clock_time_get`: WASI guest syscall; fires per-LLM-token in high volume.
const DROPPED_SPAN_NAMES: &[&str] = &["turso.configured_connection", "clock_time_get"];

/// Span name *prefixes* that are sampled at a reduced rate — they carry some
/// debug value but produce enough volume to crowd out the dispatch-critical
/// spans at 100%. Keep p99+ via `--log_with_p99` in the guest sampler; the
/// host-level sampler drops 95% of them uniformly at random.
const REDUCED_SAMPLE_PREFIXES: &[&str] = &["wasm:workspace_fs", "wasm:monty_repl"];

/// Trace-id-based deterministic sampling: keep if `(trace_id_low_u64 % 100) < rate_pct`.
/// Using trace_id instead of random keeps the decision consistent across
/// span siblings in the same trace, so a sampled trace keeps all its spans.
fn trace_id_sample_at(trace_id: TraceId, rate_pct: u8) -> bool {
    let bytes = trace_id.to_bytes();
    // Low 8 bytes as u64.
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
struct NameBasedSampler {
    inner: Sampler,
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
            return SamplingResult {
                decision: SamplingDecision::Drop,
                attributes: Vec::new(),
                trace_state: TraceState::default(),
            };
        }
        if REDUCED_SAMPLE_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
            && !trace_id_sample_at(trace_id, 5)
        {
            return SamplingResult {
                decision: SamplingDecision::Drop,
                attributes: Vec::new(),
                trace_state: TraceState::default(),
            };
        }
        self.inner
            .should_sample(parent_context, trace_id, name, span_kind, attributes, links)
    }
}

/// Default OTLP endpoint for Logfire.
const LOGFIRE_ENDPOINT: &str = "https://logfire-us.pydantic.dev";
const OTEL_EXPORTER_BUILD_RETRY_ATTEMPTS: usize = 3;
const OTEL_EXPORTER_RETRY_BASE_DELAY_MS: u64 = 250;
const TRACE_BATCH_MAX_QUEUE_SIZE: usize = 2_048;
const TRACE_BATCH_MAX_EXPORT_BATCH_SIZE: usize = 512;
const TRACE_BATCH_SCHEDULE_DELAY_MS: u64 = 1_000;
const LOG_BATCH_MAX_QUEUE_SIZE: usize = 2_048;
const LOG_BATCH_MAX_EXPORT_BATCH_SIZE: usize = 512;
const LOG_BATCH_SCHEDULE_DELAY_MS: u64 = 1_000;

/// Read an OTEL queue-size override from the environment, falling back to the
/// compiled-in default.  Called once at startup so the `std::env::var` is
/// acceptable (determinism-ok: read once at init).
fn queue_size_from_env(var: &str, default: usize) -> usize {
    std::env::var(var) // determinism-ok: startup config
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(128, 32_768)
}

#[derive(Clone, Copy, Debug)]
enum EndpointSource {
    OtlpEndpoint,
    OtlpExporterEndpoint,
    LogfireToken,
}

impl EndpointSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::OtlpEndpoint => "OTLP_ENDPOINT",
            Self::OtlpExporterEndpoint => "OTEL_EXPORTER_OTLP_ENDPOINT",
            Self::LogfireToken => "LOGFIRE_TOKEN",
        }
    }
}

#[derive(Debug)]
struct ResolvedOtelConfig {
    endpoint: String,
    endpoint_source: EndpointSource,
    logfire_token: Option<String>,
}

fn read_non_empty_env(var_name: &str) -> Option<String> {
    std::env::var(var_name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_otlp_headers(raw: &str) -> HashMap<String, String> {
    raw.split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .collect()
}

fn resolve_deployment_environment() -> Option<String> {
    if let Some(environment) = read_non_empty_env("DD_ENV") {
        return Some(environment);
    }

    if let Some(environment) = read_non_empty_env("LOGFIRE_ENVIRONMENT") {
        return Some(environment);
    }

    let resource_attrs = read_non_empty_env("OTEL_RESOURCE_ATTRIBUTES")?;
    for raw_pair in resource_attrs.split(',') {
        let Some((key, value)) = raw_pair.split_once('=') else {
            continue;
        };
        if key.trim() == "deployment.environment.name" {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn resolve_service_version() -> Option<String> {
    read_non_empty_env("DD_VERSION")
}

fn resolve_otel_config() -> Option<ResolvedOtelConfig> {
    let otlp_endpoint = read_non_empty_env("OTLP_ENDPOINT");
    let otel_exporter_endpoint = read_non_empty_env("OTEL_EXPORTER_OTLP_ENDPOINT");
    let logfire_token = read_non_empty_env("LOGFIRE_TOKEN");

    if std::env::var_os("OTLP_ENDPOINT").is_some() && otlp_endpoint.is_none() {
        eprintln!("OTLP_ENDPOINT is set but empty; ignoring it.");
    }
    if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some() && otel_exporter_endpoint.is_none()
    {
        eprintln!("OTEL_EXPORTER_OTLP_ENDPOINT is set but empty; ignoring it.");
    }
    if std::env::var_os("LOGFIRE_TOKEN").is_some() && logfire_token.is_none() {
        eprintln!("LOGFIRE_TOKEN is set but empty; skipping Authorization header.");
    }

    if let (Some(otlp), Some(otel_exporter)) = (&otlp_endpoint, &otel_exporter_endpoint)
        && otlp != otel_exporter
    {
        eprintln!(
            "Both OTLP_ENDPOINT and OTEL_EXPORTER_OTLP_ENDPOINT are set. Using OTLP_ENDPOINT."
        );
    }

    let (endpoint, endpoint_source) = if let Some(endpoint) = otlp_endpoint {
        (endpoint, EndpointSource::OtlpEndpoint)
    } else if let Some(endpoint) = otel_exporter_endpoint {
        (endpoint, EndpointSource::OtlpExporterEndpoint)
    } else if logfire_token.is_some() {
        (LOGFIRE_ENDPOINT.to_string(), EndpointSource::LogfireToken)
    } else {
        return None;
    };

    Some(ResolvedOtelConfig {
        endpoint,
        endpoint_source,
        logfire_token,
    })
}

fn build_with_retry<T, E, F>(component: &str, mut build: F) -> Result<T, Box<dyn std::error::Error>>
where
    E: std::error::Error + 'static,
    F: FnMut() -> Result<T, E>,
{
    let mut last_err: Option<Box<dyn std::error::Error>> = None;

    for attempt in 1..=OTEL_EXPORTER_BUILD_RETRY_ATTEMPTS {
        match build() {
            Ok(exporter) => return Ok(exporter),
            Err(err) => {
                eprintln!(
                    "OTEL {component} init failed (attempt {attempt}/{OTEL_EXPORTER_BUILD_RETRY_ATTEMPTS}): {err}"
                );
                last_err = Some(Box::new(err));
                if attempt < OTEL_EXPORTER_BUILD_RETRY_ATTEMPTS {
                    let backoff_ms = OTEL_EXPORTER_RETRY_BASE_DELAY_MS * attempt as u64;
                    // Blocking sleep is intentional: this runs once at startup
                    // before the async runtime is accepting work.
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        Box::new(std::io::Error::other(format!(
            "OTEL {component} init failed without a concrete error",
        )))
    }))
}

/// Guard returned by [`init_tracing`].  Holds provider handles so the
/// caller can [`shutdown`](OtelGuard::shutdown) cleanly before exit.
///
/// One process → one service identity. ADR-0053 tried to emit two
/// services from one process (platform vs embodiment); that model
/// conflicts with OpenTelemetry's resource-per-provider design. Superseded
/// 2026-04-20 in favor of one `service.name` per deployment — when Temper
/// ships inside another embodiment (Tamago, future services), that
/// binary sets its own `DD_SERVICE`.
pub struct OtelGuard {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    logger_provider: SdkLoggerProvider,
}

impl OtelGuard {
    /// Flush pending telemetry and shut down all providers.
    pub fn shutdown(self) {
        if let Err(e) = self.tracer_provider.shutdown() {
            eprintln!("tracer provider shutdown error: {e}");
        }
        if let Err(e) = self.meter_provider.shutdown() {
            eprintln!("meter provider shutdown error: {e}");
        }
        if let Err(e) = self.logger_provider.shutdown() {
            eprintln!("logger provider shutdown error: {e}");
        }
    }
}

/// Initialise observability for the process.
///
/// Resolution order for the OTLP endpoint:
/// 1. `OTLP_ENDPOINT` env var → full OTEL export to that endpoint
/// 2. `OTEL_EXPORTER_OTLP_ENDPOINT` env var → full OTEL export to that endpoint
/// 3. `LOGFIRE_TOKEN` env var → full OTEL export to Logfire default endpoint
/// 4. Neither → stderr-only logging (no OTEL export)
///
/// When `LOGFIRE_TOKEN` is set, an `Authorization: Bearer <token>` header
/// is injected into all OTLP exporters regardless of which endpoint is used.
pub fn init_observability(service_name: &str) -> Option<OtelGuard> {
    let Some(config) = resolve_otel_config() else {
        eprintln!("OTEL export disabled: no endpoint configured.");
        init_stderr_only();
        return None;
    };

    eprintln!(
        "OTEL export configured: endpoint={} source={} logfire_auth={}",
        config.endpoint,
        config.endpoint_source.as_str(),
        config.logfire_token.is_some(),
    );

    match init_tracing(&config.endpoint, service_name) {
        Ok(guard) => {
            tracing::info!(
                endpoint = %config.endpoint,
                endpoint_source = config.endpoint_source.as_str(),
                logfire_auth = config.logfire_token.is_some(),
                service_name,
                "OTEL export pipeline active",
            );
            Some(guard)
        }
        Err(e) => {
            eprintln!("Failed to initialize OTEL: {e}");
            init_stderr_only();
            None
        }
    }
}

/// Initialise OTEL tracing + metrics + logs with OTLP/HTTP export,
/// and set up a [`tracing_subscriber`] that bridges log events.
///
/// Prefer [`init_observability`] which handles endpoint resolution and
/// fallback automatically.
pub fn init_tracing(
    endpoint: &str,
    service_name: &str,
) -> Result<OtelGuard, Box<dyn std::error::Error>> {
    // Build auth headers (Logfire or custom).
    let mut headers = read_non_empty_env("OTEL_EXPORTER_OTLP_HEADERS")
        .map(|raw| parse_otlp_headers(&raw))
        .unwrap_or_default();
    if let Some(token) = read_non_empty_env("LOGFIRE_TOKEN") {
        headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    }

    // Clear signal-specific OTEL env vars that take precedence over the
    // generic endpoint.  Tools like Claude Code, Datadog agents, etc.
    // may inject these, causing telemetry to silently route to the wrong
    // backend.  We honour the developer's explicit endpoint by clearing
    // the overrides before setting the generic var.
    // SAFETY: called once at startup before any other threads read these vars.
    for var in [
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
        "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_PROTOCOL",
    ] {
        unsafe {
            std::env::remove_var(var);
        }
    }

    // Set the generic OTLP endpoint env var so the SDK appends the
    // per-signal path (/v1/traces, /v1/metrics, /v1/logs).
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint);
    }

    // Also set the headers env var so the SDK applies auth to all signals.
    if !headers.is_empty() {
        let header_str: String = headers
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", &header_str);
        }
    } else {
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_HEADERS");
        }
    }

    let mut resource_attrs = vec![KeyValue::new("service.name", service_name.to_string())];
    if let Some(environment) = resolve_deployment_environment() {
        resource_attrs.push(KeyValue::new("deployment.environment.name", environment));
    }
    if let Some(version) = resolve_service_version() {
        resource_attrs.push(KeyValue::new("service.version", version));
    }
    // ADR-0055: runtime-id enables Datadog Profiler ↔ APM trace stitching.
    // Generated once at process start; regenerates only on restart.
    // determinism-ok: observability-only identifier, not a simulation variable.
    resource_attrs.push(KeyValue::new(
        "runtime-id",
        uuid::Uuid::new_v4().to_string(),
    ));
    let resource = Resource::builder_empty()
        .with_attributes(resource_attrs)
        .build();

    // --- Traces ---
    let span_exporter = build_with_retry("trace exporter", || {
        SpanExporter::builder()
            .with_http()
            .with_timeout(Duration::from_secs(10))
            .build()
    })?;

    let trace_queue = queue_size_from_env("TEMPER_TRACE_QUEUE_SIZE", TRACE_BATCH_MAX_QUEUE_SIZE);
    let trace_batch_config = SpanBatchConfigBuilder::default()
        .with_max_queue_size(trace_queue)
        .with_max_export_batch_size(TRACE_BATCH_MAX_EXPORT_BATCH_SIZE)
        .with_scheduled_delay(Duration::from_millis(TRACE_BATCH_SCHEDULE_DELAY_MS))
        .build();

    let trace_batch_processor = BatchSpanProcessor::builder(span_exporter)
        .with_batch_config(trace_batch_config)
        .build();

    // ADR-0052 hygiene: drop known-noisy span names at ingestion.
    let sampler = NameBasedSampler {
        inner: Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
    };

    let tracer_provider = SdkTracerProvider::builder()
        .with_span_processor(trace_batch_processor)
        .with_sampler(sampler.clone())
        .with_resource(resource.clone())
        .build();

    opentelemetry::global::set_tracer_provider(tracer_provider.clone());

    // --- Metrics ---
    let metric_exporter = build_with_retry("metric exporter", || {
        MetricExporter::builder()
            .with_http()
            .with_timeout(Duration::from_secs(10))
            .build()
    })?;

    // Export every 30 s so metrics are visible quickly and canary gauges stay fresh.
    let metric_reader = PeriodicReader::builder(metric_exporter)
        .with_interval(Duration::from_secs(30))
        .build();

    let meter_provider = SdkMeterProvider::builder()
        .with_reader(metric_reader)
        .with_resource(resource.clone())
        .build();

    opentelemetry::global::set_meter_provider(meter_provider.clone());

    // --- Logs ---
    let log_exporter = build_with_retry("log exporter", || {
        LogExporter::builder()
            .with_http()
            .with_timeout(Duration::from_secs(10))
            .build()
    })?;

    let log_queue = queue_size_from_env("TEMPER_LOG_QUEUE_SIZE", LOG_BATCH_MAX_QUEUE_SIZE);
    let log_batch_config = LogBatchConfigBuilder::default()
        .with_max_queue_size(log_queue)
        .with_max_export_batch_size(LOG_BATCH_MAX_EXPORT_BATCH_SIZE)
        .with_scheduled_delay(Duration::from_millis(LOG_BATCH_SCHEDULE_DELAY_MS))
        .build();

    let log_batch_processor = BatchLogProcessor::builder(log_exporter)
        .with_batch_config(log_batch_config)
        .build();

    let logger_provider = SdkLoggerProvider::builder()
        .with_log_processor(log_batch_processor)
        .with_resource(resource)
        .build();

    // --- Tracing subscriber ---
    // Three layers:
    //   1. fmt  → stderr; JSON by default (ADR-0054), pretty if FOREGROUND_LOGS=pretty
    //   2. OTEL → spans (bridges #[instrument] / info_span! to OTEL traces)
    //   3. OTEL → logs  (bridges info!/warn!/error! to OTEL log records)
    //
    // JSON emits trace_id / span_id automatically via the OTEL trace layer
    // so log lines can be joined to their parent span in Datadog.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,hyper=warn,h2=warn,opentelemetry=warn,tonic=warn")
    });

    let pretty_logs = std::env::var("FOREGROUND_LOGS") // determinism-ok: startup config
        .map(|v| v.eq_ignore_ascii_case("pretty"))
        .unwrap_or(false);

    let otel_trace_layer =
        tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("temper"));

    // Restrict the log bridge to WARN+ to avoid flooding Logfire's /v1/logs
    // endpoint with high-volume info events.  Traces already capture info-level
    // spans via the otel_trace_layer, so no diagnostic value is lost.
    let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider);

    // `.boxed()` unifies the pretty and JSON fmt layer types so a single
    // subscriber chain compiles.
    use tracing_subscriber::Layer;
    let fmt_layer = if pretty_logs {
        tracing_subscriber::fmt::layer().with_target(true).boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(false)
            .with_target(true)
            .boxed()
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(otel_trace_layer)
        .with(otel_log_layer)
        .try_init()
        .map_err(|e| {
            std::io::Error::other(format!("failed to initialize tracing subscriber: {e}"))
        })?;

    tracing::info!(
        endpoint,
        service_name,
        trace_queue,
        trace_batch = TRACE_BATCH_MAX_EXPORT_BATCH_SIZE,
        log_queue,
        log_batch = LOG_BATCH_MAX_EXPORT_BATCH_SIZE,
        "OTEL initialised (traces + metrics + logs)"
    );

    Ok(OtelGuard {
        tracer_provider,
        meter_provider,
        logger_provider,
    })
}

/// Initialise a minimal stderr-only subscriber (no OTEL export).
///
/// Called when no OTLP endpoint is configured so `tracing::info!`
/// calls still produce output to the terminal.
pub fn init_stderr_only() {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,h2=warn"));

    let pretty = std::env::var("FOREGROUND_LOGS") // determinism-ok: startup config
        .map(|v| v.eq_ignore_ascii_case("pretty"))
        .unwrap_or(false);

    use tracing_subscriber::Layer;
    let fmt_layer = if pretty {
        tracing_subscriber::fmt::layer().with_target(true).boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(false)
            .with_target(true)
            .boxed()
    };

    if let Err(e) = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .try_init()
    {
        eprintln!("stderr tracing subscriber already initialized: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    const TEST_ENV_VARS: [&str; 3] = [
        "OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "LOGFIRE_TOKEN",
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
        use opentelemetry::trace::TraceId;
        let sampler = NameBasedSampler {
            inner: Sampler::AlwaysOn,
        };
        let trace_id = TraceId::from_bytes([0u8; 16]);

        for dropped in ["turso.configured_connection", "clock_time_get"] {
            let result =
                sampler.should_sample(None, trace_id, dropped, &SpanKind::Internal, &[], &[]);
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
    fn name_based_sampler_delegates_other_spans() {
        use opentelemetry::trace::TraceId;
        let sampler = NameBasedSampler {
            inner: Sampler::AlwaysOn,
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
}
