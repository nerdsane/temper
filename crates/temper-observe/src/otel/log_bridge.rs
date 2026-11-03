//! The bridge from `tracing` events to exported log records.

use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::registry::LookupSpan;

/// The prefix of every target the OpenTelemetry crates log their own trouble
/// under: `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp` and the rest.
const OPENTELEMETRY_TARGETS: &str = "opentelemetry";

/// The layer that exports `tracing` events as log records, except the
/// OpenTelemetry crates' own.
///
/// The SDK reports its own trouble as `tracing` events, so without this an
/// event about the export would be exported in turn. Once the logger provider
/// has shut down, every record it is given makes it warn that logs arrive after
/// shutdown. Exported, that warning is another record, which makes another
/// warning, until the stack overflows and the process aborts. On 28 Oct 2026
/// every stop of a Temper on roll 6 ended that way: 57 SIGABRT exits on 15
/// Tempers, each stop asked for. These events still reach the process's own
/// log through the fmt layer.
pub(super) fn log_bridge<S>(provider: &SdkLoggerProvider) -> impl Layer<S> + use<S>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    OpenTelemetryTracingBridge::new(provider).with_filter(filter_fn(|metadata| {
        !metadata.target().starts_with(OPENTELEMETRY_TARGETS)
    }))
}
