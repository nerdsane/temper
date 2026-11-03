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
//! | `OTEL_SERVICE_NAME` | Service name to export under (default: the name built into the binary) |
//! | `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes as `key=value,...` (default: none); attributes the server computes itself win |
//! | `OTEL_TRACES_EXPORTER`, `OTEL_METRICS_EXPORTER`, `OTEL_LOGS_EXPORTER` | `none` switches that signal's export off (default: `otlp`) |
//! | `OTEL_TRACES_SAMPLER` | `always_on`, `always_off`, `traceidratio`, `parentbased_always_on`, `parentbased_always_off` or `parentbased_traceidratio` (default: `parentbased_always_on`) |
//! | `OTEL_TRACES_SAMPLER_ARG` | Ratio from 0 to 1 for the two ratio samplers (default: 1) |
//!
//! A value that is not supported never stops the server: it is logged once at
//! startup as a warning and the default applies.

/// A copy of the caller's W3C `traceparent`, sent beside it on every call one Temper makes and read first by the
/// Temper that answers. A proxy that puts its own span into `traceparent` (Tensorlake's sandbox ingress does, and
/// never sends that span to us) leaves this copy alone, so the answering Temper still names the caller's span as
/// its parent and the trace has no gap.
pub const TRACEPARENT_COPY: &str = "x-temper-traceparent";

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{LogExporter, MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::logs::{
    BatchConfigBuilder as LogBatchConfigBuilder, BatchLogProcessor, SdkLoggerProvider,
};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{
    BatchConfigBuilder as SpanBatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider,
};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

mod config;
mod log_bridge;
mod log_time;
mod retry;
mod sampler;
mod settings;

use config::{
    parse_otlp_headers, read_non_empty_env, resolve_deployment_environment, resolve_otel_config,
    resolve_service_version,
};
use log_bridge::log_bridge;
use log_time::EventTimeLogProcessor;
use retry::{Closing, Retrying, build_with_retry};
use sampler::{
    DISPATCH_BACKGROUND_SAMPLE_RATE_DEFAULT, NameBasedSampler, TraceSamplerConfig,
    WASM_AUXILIARY_SAMPLE_RATE_DEFAULT, record_trace_sampler_config,
};
use settings::{ComputedAttributes, ExportSettings};

/// Every signal is sent as OTLP/HTTP protobuf. Named here rather than left to
/// `opentelemetry-otlp`'s default, which turns to JSON whenever any crate built
/// alongside this one enables that crate's `http-json` feature.
const OTEL_EXPORT_PROTOCOL: Protocol = Protocol::HttpBinary;
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

/// Process-lifetime runtime id shared by traces and profiler uploads.
///
/// Datadog uses this resource/tag value to stitch profiles back to the APM
/// traces captured by the same process.
pub fn runtime_id() -> &'static str {
    static RUNTIME_ID: OnceLock<String> = OnceLock::new();
    RUNTIME_ID
        .get_or_init(|| {
            read_non_empty_env("DD_RUNTIME_ID").unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        })
        .as_str()
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
    closing: Closing,
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    logger_provider: SdkLoggerProvider,
}

impl OtelGuard {
    /// Flush pending telemetry and shut down all providers. A batch that
    /// fails from here on is sent once, not again.
    pub fn shutdown(self) {
        self.closing.close();
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

    let settings = ExportSettings::from_env();
    match init_pipeline(&config.endpoint, service_name, &settings) {
        Ok(guard) => {
            tracing::info!(
                endpoint = %config.endpoint,
                endpoint_source = config.endpoint_source.as_str(),
                logfire_auth = config.logfire_token.is_some(),
                service_name = settings.service_name(service_name),
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
    init_pipeline(endpoint, service_name, &ExportSettings::from_env())
}

fn init_pipeline(
    endpoint: &str,
    service_name: &str,
    settings: &ExportSettings,
) -> Result<OtelGuard, Box<dyn std::error::Error>> {
    let service_name = settings.service_name(service_name);

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

    let environment = resolve_deployment_environment();
    let version = resolve_service_version();
    // ADR-0055: runtime-id enables Datadog Profiler ↔ APM trace stitching.
    // Generated once at process start; regenerates only on restart.
    // determinism-ok: observability-only identifier, not a simulation variable.
    let runtime_id = runtime_id().to_string();
    let computed = ComputedAttributes {
        environment,
        version,
        runtime_id,
    };
    let resource = settings.resource(service_name, computed);

    // A signal switched off keeps its provider and gets no exporter, so the
    // rest of the process sees the same tracer, meter and logger either way.
    let signals = settings.signals();
    let closing = Closing::default();

    // --- Traces ---
    // ADR-0052 hygiene: drop known-noisy span names at ingestion.
    let trace_sampler_config = TraceSamplerConfig::from_env();
    let sampler = NameBasedSampler {
        inner: settings.sampler(),
        config: trace_sampler_config.clone(),
    };

    let trace_queue = queue_size_from_env("TEMPER_TRACE_QUEUE_SIZE", TRACE_BATCH_MAX_QUEUE_SIZE);
    let mut tracer_builder = SdkTracerProvider::builder()
        .with_sampler(sampler.clone())
        .with_resource(resource.clone());
    if signals.traces {
        let span_exporter = build_with_retry("trace exporter", || {
            SpanExporter::builder()
                .with_http()
                .with_protocol(OTEL_EXPORT_PROTOCOL)
                .with_timeout(Duration::from_secs(10))
                .build()
        })?;

        let trace_batch_config = SpanBatchConfigBuilder::default()
            .with_max_queue_size(trace_queue)
            .with_max_export_batch_size(TRACE_BATCH_MAX_EXPORT_BATCH_SIZE)
            .with_scheduled_delay(Duration::from_millis(TRACE_BATCH_SCHEDULE_DELAY_MS))
            .build();

        let trace_batch_processor =
            BatchSpanProcessor::builder(Retrying::new(span_exporter, &closing))
                .with_batch_config(trace_batch_config)
                .build();
        tracer_builder = tracer_builder.with_span_processor(trace_batch_processor);
    }
    let tracer_provider = tracer_builder.build();

    opentelemetry::global::set_tracer_provider(tracer_provider.clone());

    // --- Metrics ---
    let mut meter_builder = SdkMeterProvider::builder().with_resource(resource.clone());
    if signals.metrics {
        let metric_exporter = build_with_retry("metric exporter", || {
            MetricExporter::builder()
                .with_http()
                .with_protocol(OTEL_EXPORT_PROTOCOL)
                .with_timeout(Duration::from_secs(10))
                .build()
        })?;

        // Export every 30 s so metrics are visible quickly and canary gauges stay fresh.
        let metric_reader = PeriodicReader::builder(metric_exporter)
            .with_interval(Duration::from_secs(30))
            .build();
        meter_builder = meter_builder.with_reader(metric_reader);
    }
    let meter_provider = meter_builder.build();

    opentelemetry::global::set_meter_provider(meter_provider.clone());
    record_trace_sampler_config(&trace_sampler_config);

    // --- Logs ---
    let log_queue = queue_size_from_env("TEMPER_LOG_QUEUE_SIZE", LOG_BATCH_MAX_QUEUE_SIZE);
    let mut logger_builder = SdkLoggerProvider::builder().with_resource(resource);
    if signals.logs {
        let log_exporter = build_with_retry("log exporter", || {
            LogExporter::builder()
                .with_http()
                .with_protocol(OTEL_EXPORT_PROTOCOL)
                .with_timeout(Duration::from_secs(10))
                .build()
        })?;

        let log_batch_config = LogBatchConfigBuilder::default()
            .with_max_queue_size(log_queue)
            .with_max_export_batch_size(LOG_BATCH_MAX_EXPORT_BATCH_SIZE)
            .with_scheduled_delay(Duration::from_millis(LOG_BATCH_SCHEDULE_DELAY_MS))
            .build();

        let log_batch_processor = BatchLogProcessor::builder(Retrying::new(log_exporter, &closing))
            .with_batch_config(log_batch_config)
            .build();
        logger_builder = logger_builder
            .with_log_processor(EventTimeLogProcessor)
            .with_log_processor(log_batch_processor);
    }
    let logger_provider = logger_builder.build();

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

    // Every event `env_filter` lets through, except the OpenTelemetry crates' own.
    let otel_log_layer = signals.logs.then(|| log_bridge(&logger_provider));

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

    for warning in settings.warnings() {
        tracing::warn!("OTEL export setting: {warning}");
    }
    if let Some(sampler) = settings.chosen_sampler() {
        tracing::info!(?sampler, "OTEL trace sampler set by OTEL_TRACES_SAMPLER");
    }

    tracing::info!(
        endpoint,
        service_name,
        trace_queue,
        trace_batch = TRACE_BATCH_MAX_EXPORT_BATCH_SIZE,
        trace_wasm_auxiliary_sample_pct = trace_sampler_config
            .reduced_rule_rate("wasm_auxiliary")
            .unwrap_or(WASM_AUXILIARY_SAMPLE_RATE_DEFAULT),
        trace_dispatch_background_sample_pct = trace_sampler_config
            .reduced_rule_rate("dispatch_background")
            .unwrap_or(DISPATCH_BACKGROUND_SAMPLE_RATE_DEFAULT),
        log_queue,
        log_batch = LOG_BATCH_MAX_EXPORT_BATCH_SIZE,
        "OTEL initialised ({})",
        signals.label()
    );

    Ok(OtelGuard {
        closing,
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
mod tests;
