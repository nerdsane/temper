use super::super::*;

// --- Span-hint-header extraction (ADR-0037) ---

#[test]
fn split_span_hint_headers_preserves_regular_headers() {
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("authorization".to_string(), "Bearer xyz".to_string()),
    ];
    let (kept, hints) = split_span_hint_headers(&headers);
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].0, "content-type");
    assert_eq!(kept[1].0, "authorization");
    assert!(hints.span_name.is_none());
    assert!(hints.attributes.is_empty());
}

#[test]
fn split_span_hint_headers_extracts_span_name_case_insensitive() {
    let headers = vec![
        (
            "X-Temper-Span-Name".to_string(),
            "tool.anthropic".to_string(),
        ),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    let (kept, hints) = split_span_hint_headers(&headers);
    assert_eq!(hints.span_name.as_deref(), Some("tool.anthropic"));
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].0, "content-type");
}

#[test]
fn span_hint_otel_name_prefers_semantic_name_for_datadog_resource() {
    let hints = SpanHints {
        span_name: Some("temperpaw.agent.session".to_string()),
        ..SpanHints::default()
    };

    assert_eq!(
        span_hint_otel_name(&hints, "wasm.host.http_stream"),
        "temperpaw.agent.session"
    );
    assert_eq!(
        span_hint_otel_name(&SpanHints::default(), "wasm.host.http_stream"),
        "wasm.host.http_stream"
    );
}

#[test]
fn split_span_hint_headers_extracts_generic_attributes() {
    let headers = vec![
        (
            "X-Temper-Span-Attr-gen_ai.request.model".to_string(),
            "claude-sonnet-4.6".to_string(),
        ),
        (
            "x-temper-span-attr-tool.name".to_string(),
            "temper_write".to_string(),
        ),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    let (kept, hints) = split_span_hint_headers(&headers);
    assert_eq!(kept.len(), 1);
    assert_eq!(hints.attributes.len(), 2);
    assert!(
        hints
            .attributes
            .iter()
            .any(|(k, v)| k == "gen_ai.request.model" && v == "claude-sonnet-4.6")
    );
    assert!(
        hints
            .attributes
            .iter()
            .any(|(k, v)| k == "tool.name" && v == "temper_write")
    );
}

#[test]
fn common_session_tool_and_llm_span_hints_are_datadog_visible_fields() {
    for attr_key in [
        "observability_event",
        "session_id",
        "managed_session_id",
        "inner_session_id",
        "parent_session_id",
        "agent_id",
        "environment_id",
        "entity_type",
        "entity_id",
        "action_name",
        "workflow_step",
        "tool.name",
        "tool.call_id",
        "gen_ai.operation.name",
        "gen_ai.provider.name",
        "gen_ai.request.model",
    ] {
        assert_eq!(
            datadog_visible_span_hint_field(attr_key),
            Some(attr_key),
            "{attr_key} must be recorded as a static tracing field so Datadog can facet/search it"
        );
    }

    assert_eq!(datadog_visible_span_hint_field("x_custom.future"), None);
}

#[test]
fn split_span_hint_headers_strips_empty_values() {
    let headers = vec![
        ("X-Temper-Span-Name".to_string(), "".to_string()),
        (
            "X-Temper-Span-Attr-gen_ai.request.model".to_string(),
            "".to_string(),
        ),
        ("X-Temper-Span-Attr-".to_string(), "ignored".to_string()),
    ];
    let (kept, hints) = split_span_hint_headers(&headers);
    assert!(
        kept.is_empty(),
        "all x-temper-span-* headers should be stripped"
    );
    assert!(hints.span_name.is_none(), "empty name should be ignored");
    assert!(
        hints.attributes.is_empty(),
        "empty key or value should be ignored"
    );
}

#[test]
fn split_span_hint_headers_strips_reserved_unknown_prefix() {
    // Future-proofing: unknown X-Temper-Span-* headers are stripped so they
    // don't leak to upstream services, but we don't act on them either.
    let headers = vec![
        ("X-Temper-Span-Future".to_string(), "whatever".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    let (kept, hints) = split_span_hint_headers(&headers);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].0, "content-type");
    assert!(hints.span_name.is_none());
    assert!(hints.attributes.is_empty());
}

// --- Response-capture headers (LLM Obs output) ---

#[test]
fn split_span_hint_headers_extracts_response_captures() {
    let headers = vec![
        (
            "X-Temper-Span-Capture-Response-gen_ai.completion".to_string(),
            "/content/0/text".to_string(),
        ),
        (
            "x-temper-span-capture-response-tool.result".to_string(),
            "/result".to_string(),
        ),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    let (kept, hints) = split_span_hint_headers(&headers);
    assert_eq!(kept.len(), 1);
    assert_eq!(hints.response_captures.len(), 2);
    assert!(
        hints
            .response_captures
            .iter()
            .any(|(k, v)| k == "gen_ai.completion" && v == "/content/0/text")
    );
    assert!(
        hints
            .response_captures
            .iter()
            .any(|(k, v)| k == "tool.result" && v == "/result")
    );
}

#[test]
fn split_span_hint_headers_ignores_empty_response_capture() {
    let headers = vec![
        (
            "X-Temper-Span-Capture-Response-gen_ai.completion".to_string(),
            "".to_string(),
        ),
        (
            "X-Temper-Span-Capture-Response-".to_string(),
            "/foo".to_string(),
        ),
    ];
    let (kept, hints) = split_span_hint_headers(&headers);
    assert!(
        kept.is_empty(),
        "all x-temper-span-* headers should be stripped"
    );
    assert!(hints.response_captures.is_empty());
}

#[test]
fn truncate_for_span_attr_passes_through_short_values() {
    let s = "hello world";
    assert_eq!(truncate_for_span_attr(s), s);
}

#[test]
fn truncate_for_span_attr_truncates_long_values_on_utf8_boundary() {
    // Build a string just over the budget with a multi-byte char near the cut.
    let mut s = "a".repeat(MAX_RESPONSE_CAPTURE_BYTES - 1);
    s.push('🎉'); // 4 bytes, starts at MAX - 1, extends past MAX.
    s.push_str("extra");
    let truncated = truncate_for_span_attr(&s);
    assert!(truncated.ends_with("…[truncated]"));
    assert!(
        truncated.len() <= MAX_RESPONSE_CAPTURE_BYTES + "…[truncated]".len(),
        "truncated length {} exceeded expected cap",
        truncated.len()
    );
    // Must remain valid UTF-8 (call site requires it for span attrs).
    let _ = std::str::from_utf8(truncated.as_bytes()).expect("truncated must be valid utf-8");
}

#[test]
fn apply_response_captures_is_safe_when_body_is_not_json() {
    // Should not panic on malformed JSON; call site passes through raw body.
    let span = tracing::Span::none();
    apply_response_captures(
        &span,
        "this is not JSON",
        &[(
            "gen_ai.completion".to_string(),
            "/content/0/text".to_string(),
        )],
    );
}

#[test]
fn apply_response_captures_is_safe_when_pointer_misses() {
    let span = tracing::Span::none();
    apply_response_captures(
        &span,
        r#"{"nope": "not here"}"#,
        &[(
            "gen_ai.completion".to_string(),
            "/content/0/text".to_string(),
        )],
    );
}

#[test]
fn default_http_call_batch_runs_requests_concurrently() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct ConcurrentBatchHost {
        current_in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    #[async_trait]
    impl WasmHost for ConcurrentBatchHost {
        async fn http_call(
            &self,
            method: &str,
            url: &str,
            _headers: &[(String, String)],
            _body: &str,
        ) -> Result<(u16, String), String> {
            let in_flight = self.current_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(in_flight, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(25)).await;
            self.current_in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok((200, format!("{method} {url}")))
        }

        async fn http_call_binary(
            &self,
            _method: &str,
            _url: &str,
            _headers: &[(String, String)],
            _body: &[u8],
        ) -> Result<(u16, Vec<u8>), String> {
            Err("binary calls not used in this test".to_string())
        }

        fn get_secret(&self, _key: &str) -> Result<String, String> {
            Err("secrets not used in this test".to_string())
        }

        fn log(&self, _level: &str, _message: &str) {}
    }

    let host = ConcurrentBatchHost {
        current_in_flight: AtomicUsize::new(0),
        max_in_flight: AtomicUsize::new(0),
    };

    let responses = tokio_test::block_on(host.http_call_batch(&[
        HttpBatchRequest {
            method: "GET".to_string(),
            url: "https://example.com/a".to_string(),
            headers: vec![],
            body: String::new(),
        },
        HttpBatchRequest {
            method: "GET".to_string(),
            url: "https://example.com/b".to_string(),
            headers: vec![],
            body: String::new(),
        },
        HttpBatchRequest {
            method: "GET".to_string(),
            url: "https://example.com/c".to_string(),
            headers: vec![],
            body: String::new(),
        },
    ]))
    .expect("batch call should succeed");

    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0].body, "GET https://example.com/a");
    assert!(
        host.max_in_flight.load(Ordering::SeqCst) > 1,
        "default batch implementation should overlap independent requests"
    );
}
