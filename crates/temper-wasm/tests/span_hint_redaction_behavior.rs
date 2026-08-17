//! Behavioral proof for ARN-243 / ADR-0166.
//!
//! The other tests for this gate are unit tests on the redaction helper plus
//! source-text contracts on the call sites. Neither observes what actually
//! reaches an exporter, so both would survive a change that redacts a copy of
//! the hints while the original is still recorded. This test drives a real
//! `ProductionWasmHost::http_call` with guest span-hint headers, exports the
//! resulting span through the OpenTelemetry SDK, and inspects the attributes
//! that came out the other end.
//!
//! The HTTP request itself is expected to fail — the span, its hints, and the
//! export are all complete before the socket is touched, and the assertions are
//! about the span, not the response.

use std::collections::BTreeMap;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use temper_wasm::host_trait::{ProductionWasmHost, WasmHost};
use tracing_subscriber::layer::SubscriberExt as _;

/// A port that nothing listens on, so `http_call` fails fast after the span has
/// been created and annotated.
const UNROUTABLE_URL: &str = "http://127.0.0.1:1/v1/messages";

const PROMPT: &str = "SECRET PROMPT: the user's private message";
const COMPLETION: &str = "SECRET COMPLETION: the model's private reply";

/// Guest-supplied span-hint headers carrying both content and metadata, plus a
/// content value hidden under a metadata name and a capture pointed at the body.
fn guest_hint_headers() -> Vec<(String, String)> {
    vec![
        ("content-type".to_string(), "application/json".to_string()),
        (
            "X-Temper-Span-Name".to_string(),
            "tool.llm_call".to_string(),
        ),
        (
            "X-Temper-Span-Attr-gen_ai.input.messages".to_string(),
            PROMPT.to_string(),
        ),
        (
            "X-Temper-Span-Attr-gen_ai.completion".to_string(),
            COMPLETION.to_string(),
        ),
        // A gen_ai.* name no denylist enumerates.
        (
            "X-Temper-Span-Attr-gen_ai.response.text".to_string(),
            COMPLETION.to_string(),
        ),
        // Legitimate metadata, must survive.
        (
            "X-Temper-Span-Attr-gen_ai.request.model".to_string(),
            "claude-opus-4-8".to_string(),
        ),
        // Non-LLM diagnostics, must survive.
        (
            "X-Temper-Span-Attr-provider.request_id".to_string(),
            "req_01ABC".to_string(),
        ),
        // A capture labelled as metadata but pointed at the completion body.
        (
            "X-Temper-Span-Capture-Response-gen_ai.usage.input_tokens".to_string(),
            "/content/0/text".to_string(),
        ),
    ]
}

/// Run one `http_call` under a subscriber that exports to memory, and return
/// every attribute key/value recorded on the exported spans.
fn exported_span_attributes(export_llm_content: bool) -> Vec<(String, String)> {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("arn-243-behavior");
    let subscriber =
        tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));

    tracing::subscriber::with_default(subscriber, || {
        let host =
            ProductionWasmHost::new(BTreeMap::new()).with_llm_content_export(export_llm_content);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        // The request fails (nothing listens on port 1); the span is already
        // created and annotated by then, which is what this test inspects.
        let _ =
            runtime.block_on(host.http_call("POST", UNROUTABLE_URL, &guest_hint_headers(), "{}"));
    });

    provider.force_flush().ok();
    exporter
        .get_finished_spans()
        .expect("exported spans")
        .into_iter()
        .flat_map(|span| {
            span.attributes
                .into_iter()
                .map(|kv| (kv.key.to_string(), kv.value.to_string()))
        })
        .collect()
}

#[test]
fn non_opted_in_tenant_exports_no_llm_content_on_a_real_span() {
    let attributes = exported_span_attributes(false);
    assert!(
        !attributes.is_empty(),
        "expected the http_call span to be exported with attributes"
    );

    for (key, value) in &attributes {
        assert!(
            !value.contains("SECRET PROMPT") && !value.contains("SECRET COMPLETION"),
            "LLM content reached the exporter as `{key}` = {value:?}"
        );
    }
    for content_key in [
        "gen_ai.input.messages",
        "gen_ai.completion",
        "gen_ai.response.text",
    ] {
        assert!(
            !attributes.iter().any(|(key, _)| key == content_key),
            "content attribute `{content_key}` must not be exported; got {attributes:?}"
        );
    }

    // Metadata and non-LLM diagnostics still work.
    assert!(
        attributes
            .iter()
            .any(|(key, value)| key == "gen_ai.request.model" && value == "claude-opus-4-8"),
        "model metadata must survive; got {attributes:?}"
    );
    // Documented, and deliberately asserted rather than assumed: a hint attribute
    // whose name is not in `datadog_visible_span_hint_field` does not reach the
    // exporter at all. `apply_span_hints` records those only via
    // `otel_span.set_attribute` on a context-derived handle, which does not
    // propagate back to the span the layer exports — so `provider.request_id` is
    // dropped regardless of any redaction. Two consequences worth stating: the
    // gate's real reach is narrower than the code shape suggests (it can only
    // matter for names the export path keeps), and guest diagnostics outside that
    // set are being silently lost today. The latter is an observability bug, not a
    // security one, and is tracked as ARN-350. This assertion pins the current
    // behavior so that fixing ARN-350 forces a deliberate revisit of this test.
    assert!(
        !attributes
            .iter()
            .any(|(key, _)| key == "provider.request_id"),
        "unexpected: a non-Datadog-visible hint attribute reached the exporter. If \
         ARN-350 was fixed, re-check that the redaction still covers this path; got \
         {attributes:?}"
    );
}

#[test]
fn opted_in_tenant_still_exports_llm_content_on_a_real_span() {
    let attributes = exported_span_attributes(true);
    assert!(
        attributes
            .iter()
            .any(|(key, value)| key == "gen_ai.input.messages" && value.contains("SECRET PROMPT")),
        "an opted-in tenant must still export content; got {attributes:?}"
    );
}
