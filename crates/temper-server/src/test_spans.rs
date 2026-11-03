//! Exported spans for tests: an in-memory OpenTelemetry pipeline behind the
//! tracing subscriber of the current thread.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tracing_subscriber::prelude::*;

pub(crate) struct CapturedSpans {
    exporter: InMemorySpanExporter,
    provider: SdkTracerProvider,
    _guard: tracing::subscriber::DefaultGuard,
}

impl CapturedSpans {
    /// Every span finished so far.
    pub(crate) fn finished(&self) -> Vec<SpanData> {
        let _ = self.provider.force_flush();
        self.exporter.get_finished_spans().expect("finished spans")
    }
}

/// Export every span made on this thread (and so every task of a current-thread
/// runtime) to memory until the returned value is dropped.
pub(crate) fn capture() -> CapturedSpans {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("temper-server-test")));
    let guard = tracing::subscriber::set_default(subscriber);
    CapturedSpans {
        exporter,
        provider,
        _guard: guard,
    }
}

/// The value of `span`'s attribute `key`, as text.
pub(crate) fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

/// Whether `span` carries the attribute `key`.
pub(crate) fn has_attribute(span: &SpanData, key: &str) -> bool {
    span.attributes.iter().any(|kv| kv.key.as_str() == key)
}
