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

use std::collections::HashMap;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// Default OTLP endpoint for Logfire.
const LOGFIRE_ENDPOINT: &str = "https://logfire-us.pydantic.dev";

/// Guard returned by [`init_tracing`].  Holds provider handles so the
/// caller can [`shutdown`](OtelGuard::shutdown) cleanly before exit.
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
/// 2. `LOGFIRE_TOKEN` env var → full OTEL export to Logfire default endpoint
/// 3. Neither → stderr-only logging (no OTEL export)
///
/// When `LOGFIRE_TOKEN` is set, an `Authorization: Bearer <token>` header
/// is injected into all OTLP exporters regardless of which endpoint is used.
pub fn init_observability(service_name: &str) -> Option<OtelGuard> {
    let endpoint = std::env::var("OTLP_ENDPOINT").ok().or_else(|| {
        std::env::var("LOGFIRE_TOKEN")
            .ok()
            .map(|_| LOGFIRE_ENDPOINT.to_string())
    });

    if let Some(endpoint) = endpoint {
        match init_tracing(&endpoint, service_name) {
            Ok(guard) => Some(guard),
            Err(e) => {
                eprintln!("Failed to initialize OTEL: {e}");
                init_stderr_only();
                None
            }
        }
    } else {
        init_stderr_only();
        None
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
    let mut headers = HashMap::new();
    if let Ok(token) = std::env::var("LOGFIRE_TOKEN") {
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
    }

    let resource = Resource::builder_empty()
        .with_attributes([KeyValue::new("service.name", service_name.to_string())])
        .build();

    // --- Traces ---
    let span_exporter = SpanExporter::builder()
        .with_http()
        .with_timeout(Duration::from_secs(10))
        .build()?;

    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_resource(resource.clone())
        .build();

    opentelemetry::global::set_tracer_provider(tracer_provider.clone());

    // --- Metrics ---
    let metric_exporter = MetricExporter::builder()
        .with_http()
        .with_timeout(Duration::from_secs(10))
        .build()?;

    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .with_resource(resource.clone())
        .build();

    opentelemetry::global::set_meter_provider(meter_provider.clone());

    // --- Logs ---
    let log_exporter = LogExporter::builder()
        .with_http()
        .with_timeout(Duration::from_secs(10))
        .build()?;

    let logger_provider = SdkLoggerProvider::builder()
        .with_batch_exporter(log_exporter)
        .with_resource(resource)
        .build();

    // --- Tracing subscriber ---
    // Three layers:
    //   1. fmt  → stderr (human-readable for local dev)
    //   2. OTEL → spans (bridges #[instrument] / info_span! to OTEL traces)
    //   3. OTEL → logs  (bridges info!/warn!/error! to OTEL log records)
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,hyper=warn,h2=warn,opentelemetry=warn,tonic=warn")
    });

    let fmt_layer = tracing_subscriber::fmt::layer().with_target(true);

    let otel_trace_layer =
        tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("temper"));

    let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(otel_trace_layer)
        .with(otel_log_layer)
        .init();

    tracing::info!(
        endpoint,
        service_name,
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

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .init();
}
