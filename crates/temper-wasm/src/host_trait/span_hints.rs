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

/// Longest a guest-supplied `gen_ai.*` metadata value may be when the tenant has
/// not opted into content export. Every legitimate metadata value is short — a
/// model id, a provider name, a token count, a finish reason — so this bound
/// costs nothing real while stopping a module that hides a prompt inside
/// `gen_ai.request.model`. Key names alone cannot make an untrusted value
/// metadata; the bound is what makes the allowlist mean something.
pub const MAX_REDACTED_LLM_METADATA_VALUE_BYTES: usize = 256;

/// The rule applied wherever untrusted guest telemetry can name a `gen_ai.*`
/// attribute: inside that namespace, a non-opted-in tenant keeps recognised
/// metadata keys and nothing else. Keys outside the namespace are not judged
/// here — general-purpose guest telemetry is the caller's decision, since the
/// `gen_ai.*` namespace is the part with agreed semantics that dashboards and
/// LLM Observability read.
pub(crate) fn llm_namespace_attr_allowed(key: &str) -> bool {
    let key = normalize_llm_attr_key(key);
    if !key.starts_with("gen_ai.") {
        return true;
    }
    is_llm_metadata_attr(&key)
}

/// Whether a guest-supplied key lands in the `gen_ai.*` namespace, normalized.
///
/// Every namespace test goes through this rather than `starts_with` on the raw
/// key. Normalizing in one place and matching raw in another is how the clamp and
/// the allowlist end up disagreeing: `GEN_AI.request.model` passes a normalized
/// allowlist as recognised metadata, then misses a raw-key clamp and carries a
/// whole prompt.
pub(crate) fn is_llm_namespace_key(key: &str) -> bool {
    normalize_llm_attr_key(key).starts_with("gen_ai.")
}

/// Normalize a guest-supplied attribute key before the namespace test. The
/// span-hint parser already lowercases and trims, but the guest-span and
/// wide-event payloads hand keys over verbatim, so ` GEN_AI.prompt ` would
/// otherwise miss the `gen_ai.` prefix test and pass through as ordinary
/// application telemetry.
pub(crate) fn normalize_llm_attr_key(key: &str) -> String {
    key.trim().to_ascii_lowercase()
}

/// Bound a `gen_ai.*` metadata value of any JSON shape.
///
/// Strings are clamped. Numbers and booleans pass — they are bounded by their own
/// type and are what real metadata looks like (token counts, temperature).
/// Arrays and objects are **dropped**: no metadata key in the allowlist is
/// structured, and their serialized form is unbounded, so `gen_ai.request.model =
/// ["<the whole prompt>"]` would otherwise ride straight through a clamp that only
/// matched strings. Adversarial review found exactly that hole.
pub(crate) fn clamp_llm_metadata_json(value: &serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::String(text) => Some(
            clamp_redacted_metadata_value(text)
                .map_or_else(|| value.clone(), serde_json::Value::String),
        ),
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) | serde_json::Value::Null => {
            Some(value.clone())
        }
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => None,
    }
}

/// Clamp a guest-supplied metadata value to [`MAX_REDACTED_LLM_METADATA_VALUE_BYTES`],
/// on a char boundary. Returns `None` when the value is unchanged.
pub fn clamp_redacted_metadata_value(value: &str) -> Option<String> {
    if value.len() <= MAX_REDACTED_LLM_METADATA_VALUE_BYTES {
        return None;
    }
    let mut end = MAX_REDACTED_LLM_METADATA_VALUE_BYTES;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    Some(value[..end].to_string())
}

/// Whether a key names LLM *content* (prompt, completion, system instructions,
/// tool arguments/results) under the `gen_ai.*` semantic conventions.
///
/// This is **not** the gate — enumerating content keys cannot stop a guest that
/// picks another name, which is why [`llm_namespace_attr_allowed`] allowlists
/// metadata instead. It is kept as the explicit statement of which keys are
/// content, used by the contract test that pins them against the Datadog-visible
/// field set and by the test that proves the content and metadata sets are
/// disjoint — which is why it is `#[cfg(test)]`: it documents and verifies the
/// classification, it does not enforce it. See ADR-0166.
#[cfg(test)]
pub(crate) fn is_sensitive_llm_content_attr(attr_key: &str) -> bool {
    matches!(
        attr_key,
        "gen_ai.input.messages"
            | "gen_ai.prompt"
            | "gen_ai.system_instructions"
            | "gen_ai.output.messages"
            | "gen_ai.completion"
            | "gen_ai.tool.call.arguments"
            | "gen_ai.tool.call.result"
    )
}

/// `gen_ai.*` attributes that describe a call rather than carry its content:
/// model, provider, token counts, ids, finish reasons. Everything outside this
/// set is treated as potential content for a tenant that has not opted in.
fn is_llm_metadata_attr(attr_key: &str) -> bool {
    matches!(
        attr_key,
        "gen_ai.operation.name"
            | "gen_ai.system"
            | "gen_ai.provider.name"
            | "gen_ai.conversation.id"
            | "gen_ai.request.model"
            | "gen_ai.request.max_tokens"
            | "gen_ai.request.temperature"
            | "gen_ai.response.model"
            | "gen_ai.response.finish_reasons"
            | "gen_ai.tool.name"
            | "gen_ai.tool.call.id"
            | "gen_ai.usage.input_tokens"
            | "gen_ai.usage.output_tokens"
            | "gen_ai.usage.cache_creation_input_tokens"
            | "gen_ai.usage.cache_read_input_tokens"
    )
}

/// Redact LLM content from host-captured span hints unless the tenant has opted
/// into content export. No-op when `export_content` is true. For a tenant that
/// has not opted in this:
///
/// - drops every `gen_ai.*` attribute that is not recognised metadata
///   ([`llm_namespace_attr_allowed`]), keeping attributes outside that namespace;
/// - clamps surviving `gen_ai.*` values and the guest-supplied span name to
///   [`MAX_REDACTED_LLM_METADATA_VALUE_BYTES`];
/// - clears **all** response captures, including ones named as metadata — a
///   capture's value comes from a guest-supplied JSON pointer into the response
///   body, so its name cannot establish what it holds.
///
/// See ADR-0166.
pub(crate) fn redact_llm_content_hints(hints: &mut SpanHints, export_content: bool) {
    if export_content {
        return;
    }
    // One rule, applied wherever an untrusted guest names telemetry (span hints,
    // guest spans, guest wide events): inside the `gen_ai.*` namespace only
    // recognised metadata keys survive, with bounded values. Names are chosen by
    // the guest, so enumerating content keys stops only a module that uses the
    // canonical ones — `llm.response.text` would carry the same completion.
    //
    // Attributes *outside* the namespace are left alone. This channel is the
    // generic span-hint ABI, not an LLM-only one: `provider.request_id`,
    // `rpc.method` and similar diagnostics ride on it, and dropping them would
    // remove working observability for every tenant to no security gain — a guest
    // that puts a completion in a header value it wrote itself is the free-form
    // channel named as residual in ADR-0166, the same as `host_log`.
    hints
        .attributes
        .retain(|(key, _)| llm_namespace_attr_allowed(key));

    // Names survive; values still have to be bounded, or the prompt simply
    // travels as `gen_ai.request.model`.
    for (key, value) in hints.attributes.iter_mut() {
        if is_llm_namespace_key(key)
            && let Some(clamped) = clamp_redacted_metadata_value(value)
        {
            *value = clamped;
        }
    }
    // The span *name* is guest-supplied free text on the same channel; bound it
    // for the same reason.
    if let Some(name) = hints.span_name.as_mut()
        && let Some(clamped) = clamp_redacted_metadata_value(name)
    {
        *name = clamped;
    }
    // Response captures are different in kind and are dropped in full: the value
    // is lifted out of the LLM provider's response body by a guest-supplied JSON
    // pointer, so the attribute name says nothing about what the value is. There
    // is no name that makes a body-derived value safe — `gen_ai.usage.input_tokens`
    // pointed at `/content/0/text` is a completion.
    hints.response_captures.clear();
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

#[cfg(test)]
#[path = "span_hints_test.rs"]
mod redaction_tests;
