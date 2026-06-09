use std::borrow::Cow;

use opentelemetry::KeyValue;

/// Span hints extracted from a WASM HTTP call's headers. See
/// [`split_span_hint_headers`].
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct SpanHints {
    /// If set, override the `wasm.host.http_call` span's name (from
    /// `X-Temper-Span-Name`).
    pub span_name: Option<String>,
    /// Additional span attributes to record (from `X-Temper-Span-Attr-<key>`
    /// headers). Keys are stripped of the prefix; both key and value must be
    /// non-empty.
    pub attributes: Vec<(String, String)>,
    /// Post-response captures (from `X-Temper-Span-Capture-Response-<attr>:
    /// <json-pointer>` headers). Each pair is (attr_name, RFC-6901 pointer);
    /// the host applies them after the response body is received by parsing
    /// the body as JSON and recording the value at `pointer` as span attr
    /// `attr_name` (truncated). Enables LLM Observability content capture
    /// (e.g., `gen_ai.completion` from `/content/0/text`).
    pub response_captures: Vec<(String, String)>,
}

/// Upper bound on any single span attribute value derived from a response
/// capture. Datadog's per-attribute size budget is ~25 KB; we stay well
/// under so a single attr can't dominate the span payload. Truncated values
/// are suffixed with `MAX_RESPONSE_CAPTURE_TRUNCATION_SUFFIX` so consumers
/// can tell the value was cut.
pub(crate) const MAX_RESPONSE_CAPTURE_BYTES: usize = 20 * 1024;
const MAX_RESPONSE_CAPTURE_TRUNCATION_SUFFIX: &str = "…[truncated]";

/// Split a header list into headers forwarded to the upstream request and
/// Temper-specific span hints. See ADR-0037.
///
/// WASM modules (e.g., `llm_caller`, `monty_repl`) can annotate their
/// outgoing HTTP calls with semantically meaningful span names and
/// attributes without introducing a new ABI, by prefixing headers with
/// `X-Temper-Span-`. These headers are consumed by the host and removed
/// from the outbound request.
///
/// Recognized hint headers (case-insensitive):
/// - `X-Temper-Span-Name: <name>` — sets span name (e.g., `tool.llm_call`).
/// - `X-Temper-Span-Attr-<key>: <value>` — adds `<key>=<value>` as a span
///   attribute. Useful for `gen_ai.request.model`, `tool.name`,
///   `tool.call_id`, etc.
///
/// Any `X-Temper-Span-*` header with an unrecognized suffix is stripped
/// (reserved for forward-compat) but otherwise ignored.
pub(crate) fn split_span_hint_headers(
    headers: &[(String, String)],
) -> (Vec<(String, String)>, SpanHints) {
    const PREFIX: &str = "x-temper-span-";
    const NAME_HEADER: &str = "x-temper-span-name";
    const ATTR_PREFIX: &str = "x-temper-span-attr-";
    const CAPTURE_PREFIX: &str = "x-temper-span-capture-response-";

    let mut kept: Vec<(String, String)> = Vec::with_capacity(headers.len());
    let mut hints = SpanHints::default();
    for (k, v) in headers {
        let lk = k.to_ascii_lowercase();
        if lk == NAME_HEADER {
            if !v.is_empty() {
                hints.span_name = Some(v.clone());
            }
            continue;
        }
        if let Some(attr_key) = lk.strip_prefix(ATTR_PREFIX) {
            if !attr_key.is_empty() && !v.is_empty() {
                hints.attributes.push((attr_key.to_string(), v.clone()));
            }
            continue;
        }
        if let Some(attr_key) = lk.strip_prefix(CAPTURE_PREFIX) {
            if !attr_key.is_empty() && !v.is_empty() {
                hints
                    .response_captures
                    .push((attr_key.to_string(), v.clone()));
            }
            continue;
        }
        if lk.starts_with(PREFIX) {
            // Unknown X-Temper-Span-* header: strip silently for
            // forward compatibility.
            continue;
        }
        kept.push((k.clone(), v.clone()));
    }
    (kept, hints)
}

pub(crate) fn span_hint_otel_name<'a>(
    hints: &'a SpanHints,
    default_name: &'static str,
) -> Cow<'a, str> {
    hints
        .span_name
        .as_deref()
        .map(Cow::Borrowed)
        .unwrap_or(Cow::Borrowed(default_name))
}

pub(crate) fn datadog_visible_span_hint_field(attr_key: &str) -> Option<&'static str> {
    match attr_key {
        "tenant" => Some("tenant"),
        "wasm_module" => Some("wasm_module"),
        "observability_event" => Some("observability_event"),
        "session_id" => Some("session_id"),
        "managed_session_id" => Some("managed_session_id"),
        "inner_session_id" => Some("inner_session_id"),
        "parent_session_id" => Some("parent_session_id"),
        "agent_id" => Some("agent_id"),
        "environment_id" => Some("environment_id"),
        "entity_type" => Some("entity_type"),
        "entity_id" => Some("entity_id"),
        "trigger_action" => Some("trigger_action"),
        "action_name" => Some("action_name"),
        "workflow_step" => Some("workflow_step"),
        "workflow_root_entity_type" => Some("workflow_root_entity_type"),
        "workflow_root_entity_id" => Some("workflow_root_entity_id"),
        "workflow_run_id" => Some("workflow_run_id"),
        "dd_llmobs_enabled" => Some("dd_llmobs_enabled"),
        "tool.name" => Some("tool.name"),
        "tool.call_id" => Some("tool.call_id"),
        "gen_ai.operation.name" => Some("gen_ai.operation.name"),
        "gen_ai.provider.name" => Some("gen_ai.provider.name"),
        "gen_ai.system" => Some("gen_ai.system"),
        "gen_ai.request.model" => Some("gen_ai.request.model"),
        "gen_ai.response.model" => Some("gen_ai.response.model"),
        "gen_ai.request.temperature" => Some("gen_ai.request.temperature"),
        "gen_ai.request.max_tokens" => Some("gen_ai.request.max_tokens"),
        "gen_ai.conversation.id" => Some("gen_ai.conversation.id"),
        "gen_ai.system_instructions" => Some("gen_ai.system_instructions"),
        "gen_ai.input.messages" => Some("gen_ai.input.messages"),
        "gen_ai.prompt" => Some("gen_ai.prompt"),
        "gen_ai.output.messages" => Some("gen_ai.output.messages"),
        "gen_ai.completion" => Some("gen_ai.completion"),
        "gen_ai.response.finish_reasons" => Some("gen_ai.response.finish_reasons"),
        "gen_ai.usage.input_tokens" => Some("gen_ai.usage.input_tokens"),
        "gen_ai.usage.output_tokens" => Some("gen_ai.usage.output_tokens"),
        "gen_ai.usage.cache_read_input_tokens" => Some("gen_ai.usage.cache_read_input_tokens"),
        "gen_ai.usage.cache_creation_input_tokens" => {
            Some("gen_ai.usage.cache_creation_input_tokens")
        }
        "http.response.body.size" => Some("http.response.body.size"),
        _ => None,
    }
}

/// Apply span hints to the currently-active tracing span's underlying OTel
/// span. `update_name` overrides the span display name; `set_attribute`
/// calls attach the key/value pairs. If no tracer subscriber is installed
/// (tests, early startup), this is a no-op.
pub(crate) fn apply_span_hints(span: &tracing::Span, hints: &SpanHints) {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let cx = span.context();
    let otel_span = cx.span();
    if let Some(ref name) = hints.span_name {
        span.record("otel.name", name.as_str());
        otel_span.update_name(name.clone());
    }
    for (k, v) in &hints.attributes {
        if let Some(field_name) = datadog_visible_span_hint_field(k) {
            span.record(field_name, v.as_str());
        }
        otel_span.set_attribute(KeyValue::new(k.clone(), v.clone()));
    }
}

/// Resolve each `(attr, json_pointer)` pair from `response_captures` against
/// `response_body`, and record the resolved value as a span attribute.
///
/// Used to capture LLM response content (`gen_ai.completion`) onto the
/// `tool.llm_call.*` span so DD LLM Observability populates its Output
/// panel. Request-side attrs (`gen_ai.prompt`, model, system) are handled
/// by the request-header path; this function is the post-response half.
///
/// Behaviour:
/// - Non-JSON `response_body` -> silently skipped (not all captures succeed;
///   the call may have errored upstream and returned HTML, empty, etc.).
/// - Pointer missing in body -> attr not recorded (no panic, no error log).
/// - String values passed through; non-string (number, object, array) are
///   re-serialized via `serde_json`.
/// - Values over [`MAX_RESPONSE_CAPTURE_BYTES`] are truncated at a UTF-8
///   boundary with [`MAX_RESPONSE_CAPTURE_TRUNCATION_SUFFIX`] appended.
pub(crate) fn apply_response_captures(
    span: &tracing::Span,
    response_body: &str,
    response_captures: &[(String, String)],
) {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    if response_captures.is_empty() {
        return;
    }
    let Ok(root) = serde_json::from_str::<serde_json::Value>(response_body) else {
        return;
    };

    let cx = span.context();
    let otel_span = cx.span();
    for (attr, pointer) in response_captures {
        let Some(value) = root.pointer(pointer) else {
            continue;
        };
        let raw = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let truncated = truncate_for_span_attr(&raw);
        otel_span.set_attribute(KeyValue::new(attr.clone(), truncated));
    }
}

/// Truncate `value` to at most [`MAX_RESPONSE_CAPTURE_BYTES`] bytes on a
/// UTF-8 boundary, appending a marker suffix if truncated.
pub(crate) fn truncate_for_span_attr(value: &str) -> String {
    if value.len() <= MAX_RESPONSE_CAPTURE_BYTES {
        return value.to_string();
    }
    let mut cut = MAX_RESPONSE_CAPTURE_BYTES;
    while cut > 0 && !value.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + MAX_RESPONSE_CAPTURE_TRUNCATION_SUFFIX.len());
    out.push_str(&value[..cut]);
    out.push_str(MAX_RESPONSE_CAPTURE_TRUNCATION_SUFFIX);
    out
}
