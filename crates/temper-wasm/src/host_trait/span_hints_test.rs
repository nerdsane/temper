//! Redaction contract for span hints (ARN-243 / ADR-0166).
use super::*;

fn content_hints() -> SpanHints {
    SpanHints {
        span_name: None,
        attributes: vec![
            ("gen_ai.request.model".to_string(), "claude".to_string()),
            (
                "gen_ai.input.messages".to_string(),
                "SECRET PROMPT".to_string(),
            ),
            (
                "gen_ai.system_instructions".to_string(),
                "SECRET SYSTEM".to_string(),
            ),
            ("tenant".to_string(), "acme".to_string()),
        ],
        response_captures: vec![
            (
                "gen_ai.completion".to_string(),
                "/content/0/text".to_string(),
            ),
            ("http.response.body.size".to_string(), "/size".to_string()),
        ],
    }
}

#[test]
fn classifies_content_vs_metadata_attrs() {
    assert!(is_sensitive_llm_content_attr("gen_ai.input.messages"));
    assert!(is_sensitive_llm_content_attr("gen_ai.completion"));
    assert!(is_sensitive_llm_content_attr("gen_ai.tool.call.arguments"));
    assert!(!is_sensitive_llm_content_attr("gen_ai.request.model"));
    assert!(!is_sensitive_llm_content_attr("gen_ai.usage.input_tokens"));
    assert!(!is_sensitive_llm_content_attr("tenant"));
}

/// Guards the content-key list against the open export surface (`apply_span_hints`
/// records every surviving hint). Every content-bearing span field must be
/// denied; every metadata field Datadog shows must survive. A denylist gap
/// here is a leak; a false positive is over-redaction. See ADR-0166.
#[test]
fn every_content_key_is_a_recognized_datadog_visible_field() {
    // Raw LLM content that reaches a span. The first five are also in the
    // `datadog_visible_span_hint_field` allowlist; the two tool fields reach
    // Datadog via the unconditional `set_attribute` path. All must be denied.
    let content_fields = [
        "gen_ai.system_instructions",
        "gen_ai.input.messages",
        "gen_ai.prompt",
        "gen_ai.output.messages",
        "gen_ai.completion",
        "gen_ai.tool.call.arguments",
        "gen_ai.tool.call.result",
    ];
    for field in content_fields {
        assert!(
            is_sensitive_llm_content_attr(field),
            "content field `{field}` must be redacted (denylist gap = leak)"
        );
    }

    // Datadog-visible metadata must never be redacted (no over-redaction),
    // and each must be a real recognized field (guards typos in this list).
    let metadata_fields = [
        "gen_ai.request.model",
        "gen_ai.response.model",
        "gen_ai.provider.name",
        "gen_ai.system",
        "gen_ai.operation.name",
        "gen_ai.request.temperature",
        "gen_ai.request.max_tokens",
        "gen_ai.conversation.id",
        "gen_ai.response.finish_reasons",
        "gen_ai.usage.input_tokens",
        "gen_ai.usage.output_tokens",
        "gen_ai.usage.cache_read_input_tokens",
        "gen_ai.usage.cache_creation_input_tokens",
        "tenant",
        "session_id",
        "agent_id",
    ];
    for field in metadata_fields {
        assert!(
            !is_sensitive_llm_content_attr(field),
            "metadata field `{field}` must NOT be redacted (over-redaction)"
        );
        assert!(
            datadog_visible_span_hint_field(field).is_some(),
            "metadata field `{field}` should be a recognized Datadog-visible field"
        );
    }

    // Every Datadog-visible content field must also be denied.
    for field in [
        "gen_ai.system_instructions",
        "gen_ai.input.messages",
        "gen_ai.prompt",
        "gen_ai.output.messages",
        "gen_ai.completion",
    ] {
        assert!(
            datadog_visible_span_hint_field(field).is_some(),
            "content field `{field}` should be a recognized Datadog-visible field"
        );
    }
}

#[test]
fn redacts_sensitive_content_hints_when_not_opted_in() {
    let mut hints = content_hints();
    redact_llm_content_hints(&mut hints, false);

    // Content attributes stripped.
    assert!(
        hints
            .attributes
            .iter()
            .all(|(k, _)| k != "gen_ai.input.messages"),
        "prompt attr must be redacted"
    );
    assert!(
        hints
            .attributes
            .iter()
            .all(|(k, _)| k != "gen_ai.system_instructions"),
        "system instructions attr must be redacted"
    );
    // Metadata attributes preserved.
    assert!(
        hints
            .attributes
            .iter()
            .any(|(k, _)| k == "gen_ai.request.model"),
        "model metadata must survive"
    );
    assert!(hints.attributes.iter().any(|(k, _)| k == "tenant"));

    // Every response capture is dropped, whatever it is named. A capture is
    // `(attribute_name, json_pointer)` and the value is lifted straight out of
    // the provider's response body, so the name is a guest-chosen label with no
    // bearing on whether the value is content.
    assert!(
        hints.response_captures.is_empty(),
        "no body-derived capture may survive for a non-opted-in tenant, got {:?}",
        hints.response_captures
    );
}

/// The content and metadata sets must stay disjoint: a key in both would make
/// the allowlist export content, and the doc on `is_sensitive_llm_content_attr`
/// claims they are separate.
#[test]
fn content_and_metadata_key_sets_are_disjoint() {
    for key in [
        "gen_ai.input.messages",
        "gen_ai.prompt",
        "gen_ai.system_instructions",
        "gen_ai.output.messages",
        "gen_ai.completion",
        "gen_ai.tool.call.arguments",
        "gen_ai.tool.call.result",
    ] {
        assert!(is_sensitive_llm_content_attr(key));
        assert!(
            !is_llm_metadata_attr(key),
            "`{key}` is content and must not also be metadata"
        );
        assert!(
            !llm_namespace_attr_allowed(key),
            "`{key}` must not pass the namespace allowlist"
        );
    }
}

/// Guest-span and wide-event payloads hand keys over verbatim, so the
/// namespace test has to normalize or `GEN_AI.prompt` is read as ordinary
/// application telemetry and passes straight through.
#[test]
fn namespace_test_normalizes_guest_supplied_keys() {
    for key in [
        "GEN_AI.prompt",
        " gen_ai.prompt ",
        "Gen_Ai.Completion",
        "\tGEN_AI.INPUT.MESSAGES",
    ] {
        assert!(
            !llm_namespace_attr_allowed(key),
            "`{key}` must be recognized as an unlisted gen_ai.* key"
        );
    }
    assert!(llm_namespace_attr_allowed("GEN_AI.request.model"));
    assert!(llm_namespace_attr_allowed("order.id"));
}

/// The gate must not be bypassable inside the namespace it protects. Guest
/// modules choose the attribute names (`X-Temper-Span-Attr-*`) and the capture
/// pointers, so a denylist of canonical `gen_ai.*` content keys is defeated by
/// picking another name in that namespace for the same value.
#[test]
fn redaction_is_not_bypassable_by_guest_chosen_names() {
    let mut hints = SpanHints::default();
    // Same completion text, under gen_ai.* names no denylist would carry.
    hints.attributes.push((
        "gen_ai.response.text".to_string(),
        "SECRET COMPLETION".to_string(),
    ));
    hints
        .attributes
        .push(("gen_ai.debug.dump".to_string(), "SECRET PROMPT".to_string()));
    // A capture labelled as innocuous metadata, pointed at the completion body.
    hints.response_captures.push((
        "gen_ai.usage.input_tokens".to_string(),
        "/content/0/text".to_string(),
    ));

    redact_llm_content_hints(&mut hints, false);

    assert!(
        hints.attributes.is_empty(),
        "an unrecognized gen_ai.* name must not survive on the strength of \
         being unlisted, got {:?}",
        hints.attributes
    );
    assert!(
        hints.response_captures.is_empty(),
        "a body-derived capture must not survive on the strength of its name"
    );
}

/// The span-hint channel is the generic observability ABI, not an LLM-only
/// one. Redacting must not quietly delete a module's ordinary diagnostics for
/// every tenant — the opt-in list is empty by default, so over-redaction here
/// would be the default behavior for everyone.
#[test]
fn redaction_keeps_non_llm_diagnostics() {
    let mut hints = SpanHints::default();
    for (key, value) in [
        ("provider.request_id", "req_01ABC"),
        ("rpc.method", "Complete"),
        ("http.response.status_code", "200"),
        ("tenant", "acme"),
    ] {
        hints.attributes.push((key.to_string(), value.to_string()));
    }

    redact_llm_content_hints(&mut hints, false);

    for key in [
        "provider.request_id",
        "rpc.method",
        "http.response.status_code",
        "tenant",
    ] {
        assert!(
            hints.attributes.iter().any(|(k, _)| k == key),
            "non-LLM diagnostic `{key}` must keep working for a non-opted-in \
             tenant; got {:?}",
            hints.attributes
        );
    }
}

/// Names pass the allowlist; values must still be bounded, or the prompt
/// simply travels as `gen_ai.request.model`. The guest-supplied span name is
/// free text on the same channel and is bounded for the same reason.
#[test]
fn metadata_values_and_span_name_are_bounded_when_not_opted_in() {
    let mut hints = SpanHints {
        span_name: Some("N".repeat(4096)),
        ..SpanHints::default()
    };
    hints
        .attributes
        .push(("gen_ai.request.model".to_string(), "M".repeat(4096)));

    redact_llm_content_hints(&mut hints, false);

    let (_, model) = hints
        .attributes
        .iter()
        .find(|(k, _)| k == "gen_ai.request.model")
        .expect("recognized metadata survives");
    assert!(
        model.len() <= MAX_REDACTED_LLM_METADATA_VALUE_BYTES,
        "metadata value must be clamped, got {} bytes",
        model.len()
    );
    assert!(
        hints.span_name.as_deref().unwrap_or_default().len()
            <= MAX_REDACTED_LLM_METADATA_VALUE_BYTES,
        "span name must be clamped"
    );
}

/// Clamping must not split a multi-byte character.
#[test]
fn clamping_respects_char_boundaries() {
    let value = "é".repeat(1024);
    let clamped = clamp_redacted_metadata_value(&value).expect("should clamp");
    assert!(clamped.len() <= MAX_REDACTED_LLM_METADATA_VALUE_BYTES);
    assert!(value.starts_with(&clamped), "clamp must be a prefix");
    assert!(clamp_redacted_metadata_value("short").is_none());
}

#[test]
fn keeps_content_hints_when_opted_in() {
    let mut hints = content_hints();
    redact_llm_content_hints(&mut hints, true);
    assert!(
        hints
            .attributes
            .iter()
            .any(|(k, _)| k == "gen_ai.input.messages"),
        "opted-in tenant keeps prompt"
    );
    assert!(
        hints
            .response_captures
            .iter()
            .any(|(k, _)| k == "gen_ai.completion"),
        "opted-in tenant keeps completion"
    );
}

/// The clamp and the allowlist must agree on what "in the namespace" means.
/// If the allowlist normalizes the key but the clamp matches the raw one,
/// `GEN_AI.request.model` passes as recognised metadata and then skips the
/// clamp — carrying an entire prompt under a metadata name.
#[test]
fn namespace_clamp_and_allowlist_agree_on_unnormalized_keys() {
    let prompt = "P".repeat(4096);
    let mut hints = SpanHints::default();
    hints
        .attributes
        .push(("GEN_AI.request.model".to_string(), prompt.clone()));
    hints
        .attributes
        .push((" gen_ai.request.model ".to_string(), prompt.clone()));

    redact_llm_content_hints(&mut hints, false);

    for (key, value) in &hints.attributes {
        assert!(
            value.len() <= MAX_REDACTED_LLM_METADATA_VALUE_BYTES,
            "`{key}` escaped the clamp with {} bytes — the allowlist and the clamp \
             disagree about the gen_ai.* namespace",
            value.len()
        );
    }
}
