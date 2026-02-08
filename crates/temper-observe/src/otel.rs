//! OTEL setup and teardown for Temper observability.
//!
//! Configures a [`TracerProvider`] and [`MeterProvider`] with OTLP HTTP
//! exporters.  When no OTLP endpoint is configured the global no-op
//! providers remain active, so `emit_span` / `emit_metrics` silently
//! discard data — no conditional logic needed at call sites.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use opentelemetry_otlp::{MetricExporter, SpanExporter, WithExportConfig};

/// Guard returned by [`init_tracing`].  Holds provider handles so the
/// caller can [`shutdown`] cleanly before exit.
pub struct OtelGuard {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
}

impl OtelGuard {
    /// Flush pending telemetry and shut down both providers.
    pub fn shutdown(self) {
        if let Err(e) = self.tracer_provider.shutdown() {
            tracing::warn!(error = %e, "tracer provider shutdown error");
        }
        if let Err(e) = self.meter_provider.shutdown() {
            tracing::warn!(error = %e, "meter provider shutdown error");
        }
    }
}

/// Initialise OTEL tracing + metrics with OTLP/HTTP export.
///
/// `endpoint` is the OTLP collector base URL, e.g.
/// `http://localhost:4318`.  This function sets the
/// `OTEL_EXPORTER_OTLP_ENDPOINT` env var so the SDK appends
/// `/v1/traces` and `/v1/metrics` signal paths automatically.
///
/// Returns an [`OtelGuard`] that **must** be kept alive for the
/// duration of the process and shut down before exit.
pub fn init_tracing(endpoint: &str, service_name: &str) -> Result<OtelGuard, Box<dyn std::error::Error>> {
    // Set the generic OTLP endpoint env var so the SDK appends the
    // per-signal path (/v1/traces, /v1/metrics).  `with_endpoint()`
    // expects the full URL including the signal path, but the env var
    // approach handles it correctly.
    // SAFETY: called once at startup before any other threads read this var.
    unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint); }

    let resource = Resource::builder_empty()
        .with_attributes([
            KeyValue::new("service.name", service_name.to_string()),
        ])
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
        .with_resource(resource)
        .build();

    opentelemetry::global::set_meter_provider(meter_provider.clone());

    tracing::info!(endpoint, service_name, "OTEL tracing initialised");

    Ok(OtelGuard {
        tracer_provider,
        meter_provider,
    })
}
