use super::*;
use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::prelude::*;

#[test]
fn llm_content_export_defaults_to_redact_and_is_opt_in() {
    use std::collections::BTreeMap;
    // Fail-safe: a host built without an explicit opt-in must redact LLM
    // content, so any construction site that forgets `.with_llm_content_export`
    // still defaults to safe. See ADR-0166 (ARN-243).
    let default_host = ProductionWasmHost::new(BTreeMap::new());
    assert!(
        !default_host.export_llm_content,
        "host must default to redacting LLM content"
    );

    let opted_in = ProductionWasmHost::new(BTreeMap::new()).with_llm_content_export(true);
    assert!(
        opted_in.export_llm_content,
        "with_llm_content_export(true) must opt in"
    );

    let opted_out = ProductionWasmHost::new(BTreeMap::new()).with_llm_content_export(false);
    assert!(
        !opted_out.export_llm_content,
        "with_llm_content_export(false) must redact"
    );
}

#[test]
fn guest_metric_count_kind_is_counter() {
    assert!(guest_metric_is_counter_kind(Some("count")));
    assert!(guest_metric_is_counter_kind(Some("counter")));
    assert!(!guest_metric_is_counter_kind(Some("histogram")));
    assert!(!guest_metric_is_counter_kind(None));
}

#[test]
fn guest_metric_tags_reject_high_cardinality_correlation_ids() {
    assert!(guest_metric_tag_allowed("provider"));
    assert!(guest_metric_tag_allowed("model"));
    assert!(!guest_metric_tag_allowed("trace_id"));
    assert!(!guest_metric_tag_allowed("dd.span_id"));
    assert!(!guest_metric_tag_allowed("session_id"));
    assert!(!guest_metric_tag_allowed("workflow_run_id"));
    assert!(!guest_metric_tag_allowed("tool.call_id"));
}

#[test]
fn production_host_uses_lazy_secret_resolver_for_missing_eager_key() {
    let requested = Arc::new(Mutex::new(Vec::new()));
    let requested_for_resolver = Arc::clone(&requested);
    let resolver: SecretResolverFn = Arc::new(move |key| {
        requested_for_resolver
            .lock()
            .expect("requested keys lock poisoned")
            .push(key.to_string());
        Ok(format!("lazy:{key}"))
    });
    let host = ProductionWasmHost::new(BTreeMap::new()).with_secret_resolver(resolver);

    assert_eq!(
        host.get_secret("provider_api_key"),
        Ok("lazy:provider_api_key".to_string())
    );
    assert_eq!(
        *requested.lock().expect("requested keys lock poisoned"),
        vec!["provider_api_key".to_string()]
    );
}

#[test]
fn production_host_uses_lazy_resolver_for_guest_lookup_even_when_eager_key_exists() {
    let requested = Arc::new(Mutex::new(Vec::<String>::new()));
    let requested_for_resolver = Arc::clone(&requested);
    let resolver: SecretResolverFn = Arc::new(move |key| {
        requested_for_resolver
            .lock()
            .expect("requested keys lock poisoned")
            .push(key.to_string());
        Ok(format!("lazy:{key}"))
    });
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "blob_endpoint".to_string(),
        "https://blob.example".to_string(),
    );
    let host = ProductionWasmHost::new(secrets).with_secret_resolver(resolver);

    assert_eq!(
        host.get_secret("blob_endpoint"),
        Ok("lazy:blob_endpoint".to_string())
    );
    assert_eq!(
        *requested.lock().expect("requested keys lock poisoned"),
        vec!["blob_endpoint".to_string()]
    );
}

/// Build a Connect frame: [flags(1)][length(4 big-endian)][payload].
fn make_frame(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(flags);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

#[test]
fn parse_single_data_frame() {
    let payload = b"{\"stdout\":\"hello\"}";
    let data = make_frame(0x00, payload);
    let frames = parse_connect_frames(&data).expect("single data frame should parse");
    assert_eq!(frames, vec!["{\"stdout\":\"hello\"}"]);
}

#[test]
fn parse_multiple_frames() {
    let mut data = make_frame(0x00, b"{\"stdout\":\"line1\"}");
    data.extend(make_frame(0x00, b"{\"stdout\":\"line2\"}"));
    data.extend(make_frame(0x02, b"trailer")); // trailer frame, should be skipped
    let frames = parse_connect_frames(&data).expect("multiple connect frames should parse");
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], "{\"stdout\":\"line1\"}");
    assert_eq!(frames[1], "{\"stdout\":\"line2\"}");
}

#[test]
fn parse_empty_input() {
    let frames = parse_connect_frames(&[]).expect("empty input should parse");
    assert!(frames.is_empty());
}

#[test]
fn encode_connect_json_frame_wraps_payload() {
    let payload = "{\"hello\":\"world\"}";
    let framed = encode_connect_json_frame(payload);
    assert_eq!(framed[0], 0x00);
    assert_eq!(
        u32::from_be_bytes([framed[1], framed[2], framed[3], framed[4]]) as usize,
        payload.len()
    );
    assert_eq!(&framed[5..], payload.as_bytes());
}

#[test]
fn parse_trailer_only() {
    let data = make_frame(0x02, b"{}");
    let frames = parse_connect_frames(&data).expect("trailer-only frame should parse");
    assert!(frames.is_empty());
}

#[test]
fn parse_incomplete_header_errors() {
    let result = parse_connect_frames(&[0x00, 0x00]);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .contains("incomplete Connect frame header")
    );
}

#[test]
fn parse_incomplete_payload_errors() {
    // Header says 100 bytes but only 3 available
    let mut data = vec![0x00];
    data.extend_from_slice(&100u32.to_be_bytes());
    data.extend_from_slice(b"abc");
    let result = parse_connect_frames(&data);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .contains("incomplete Connect frame payload")
    );
}

#[test]
fn current_traceparent_header_prefers_active_span_context() {
    let tracer_provider = SdkTracerProvider::builder().build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("temper-wasm-test")),
    );
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let span = tracing::info_span!("wasm.reply");
    let expected = {
        let _guard = span.enter();
        let span_context = tracing::Span::current()
            .context()
            .span()
            .span_context()
            .clone();
        assert!(
            span_context.is_valid(),
            "test span should have an OTEL context"
        );
        format!(
            "00-{}-{}-01",
            span_context.trace_id(),
            span_context.span_id()
        )
    };

    let actual = span
        .in_scope(|| current_traceparent_header(&tracing::Span::current(), None))
        .expect("active span should produce a traceparent");
    assert_eq!(actual, expected);
}

#[test]
fn internal_http_call_replaces_guest_authority_with_fresh_capability() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_clone = Arc::clone(&captured);

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut buf = [0u8; 8192];
        let len = stream.read(&mut buf).expect("read request");
        *captured_clone.lock().expect("capture lock") =
            String::from_utf8_lossy(&buf[..len]).into_owned();

        let body = "{}";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write response");
    });

    let host = ProductionWasmHost::new(BTreeMap::new())
        .with_internal_api_base_url(Some(format!("http://{addr}")))
        .with_internal_capability_issuer(Arc::new(|method, url| {
            assert_eq!(method, "GET");
            assert!(url.ends_with("/tdata/Directories"));
            InternalHttpCapability::new("request-capability".to_string(), "tenant-a".to_string())
        }))
        .with_invocation_context(WasmInvocationContext {
            tenant: "tenant-a".to_string(),
            entity_type: "Workspace".to_string(),
            entity_id: "ws-1".to_string(),
            trigger_action: "CreateFile".to_string(),
            wasm_module: Some("workspace_fs".to_string()),
            trigger_params: Value::Null,
            entity_state: Value::Null,
            agent_id: Some("operator".to_string()),
            session_id: None,
            integration_config: BTreeMap::new(),
            trace_id: String::new(),
            workflow_root_entity_type: Some("CurationQuery".to_string()),
            workflow_root_entity_id: Some("cq-1".to_string()),
            workflow_run_id: Some("CurationQuery:cq-1".to_string()),
            http_request: None,
        });

    let headers = vec![
        ("Authorization".to_string(), "Bearer guest-root".to_string()),
        ("X-Tenant-Id".to_string(), "victim".to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("x-temper-principal-id".to_string(), "attacker".to_string()),
        ("x-temper-attr-limit".to_string(), "999".to_string()),
        ("x-regular".to_string(), "preserved".to_string()),
    ];

    let (status, _) = tokio_test::block_on(host.http_call(
        "GET",
        &format!("http://{addr}/tdata/Directories"),
        &headers,
        "",
    ))
    .expect("internal call should succeed");

    assert_eq!(status, 200);
    server.join().expect("server thread");

    let request = captured.lock().expect("capture lock").to_lowercase();
    assert!(
        request.contains("authorization: bearer request-capability"),
        "expected bearer token in request, got: {request}"
    );
    assert!(
        request.contains("x-tenant-id: tenant-a"),
        "expected capability tenant in request, got: {request}"
    );
    assert!(!request.contains("guest-root"), "{request}");
    assert!(!request.contains("victim"), "{request}");
    assert!(!request.contains("x-temper-principal"), "{request}");
    assert!(!request.contains("x-temper-attr"), "{request}");
    assert!(request.contains("x-regular: preserved"), "{request}");
    assert!(
        request.contains("x-temper-workflow-root-entity-type: curationquery"),
        "expected workflow root type header, got: {request}"
    );
    assert!(
        request.contains("x-temper-workflow-root-entity-id: cq-1"),
        "expected workflow root id header, got: {request}"
    );
    assert!(
        request.contains("x-temper-workflow-run-id: curationquery:cq-1"),
        "expected workflow run id header, got: {request}"
    );
}

#[test]
fn internal_http_call_without_issuer_fails_before_network() {
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "temper_api_url".to_string(),
        "http://127.0.0.1:9".to_string(),
    );
    secrets.insert("temper_api_key".to_string(), "ambient-root".to_string());
    let host = ProductionWasmHost::new(secrets)
        .with_internal_api_base_url(Some("http://127.0.0.1:9".to_string()));

    let error = tokio_test::block_on(host.http_call(
        "GET",
        "http://127.0.0.1:9/tdata/Directories",
        &[],
        "",
    ))
    .expect_err("internal calls without an issuer must fail closed");
    assert!(
        error.contains("no authenticated capability issuer"),
        "{error}"
    );
}

#[test]
fn tenant_secret_cannot_reclassify_an_external_origin_as_internal() {
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "temper_api_url".to_string(),
        "http://attacker.example".to_string(),
    );
    let issuer_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls_for_issuer = Arc::clone(&issuer_calls);
    let host =
        ProductionWasmHost::new(secrets).with_internal_capability_issuer(Arc::new(move |_, _| {
            calls_for_issuer.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            InternalHttpCapability::new("must-not-issue".to_string(), "tenant-a".to_string())
        }));

    assert!(!host.is_internal_temper_url("http://attacker.example/tdata/Orders"));
    assert_eq!(issuer_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn internal_binary_http_call_sanitizes_and_injects_capability() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_for_server = Arc::clone(&captured);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 8_192];
        let read = stream.read(&mut buffer).expect("read request");
        request.extend_from_slice(&buffer[..read]);
        *captured_for_server.lock().expect("capture lock") =
            String::from_utf8_lossy(&request).into_owned();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .expect("write response");
    });

    let host = ProductionWasmHost::new(BTreeMap::new())
        .with_internal_api_base_url(Some(format!("http://{addr}")))
        .with_internal_capability_issuer(Arc::new(|method, _| {
            assert_eq!(method, "PUT");
            InternalHttpCapability::new("binary-capability".to_string(), "tenant-b".to_string())
        }));
    let (status, body) = tokio_test::block_on(host.http_call_binary(
        "PUT",
        &format!("http://{addr}/api/blob?part=1"),
        &[
            ("authorization".to_string(), "Bearer guest".to_string()),
            ("x-tenant-id".to_string(), "victim".to_string()),
            ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ],
        b"bytes",
    ))
    .expect("binary internal request should succeed");
    assert_eq!(status, 200);
    assert_eq!(body, b"ok");
    server.join().expect("server should finish");

    let request = captured.lock().expect("capture lock").to_lowercase();
    assert!(
        request.contains("authorization: bearer binary-capability"),
        "{request}"
    );
    assert!(request.contains("x-tenant-id: tenant-b"), "{request}");
    assert!(!request.contains("bearer guest"), "{request}");
    assert!(!request.contains("victim"), "{request}");
    assert!(!request.contains("x-temper-principal"), "{request}");
}

#[test]
fn internal_capability_requests_do_not_follow_redirects() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut buffer = [0_u8; 8_192];
        let _ = stream.read(&mut buffer).expect("read request");
        stream
            .write_all(
                b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .expect("write redirect");
    });

    let host = ProductionWasmHost::new(BTreeMap::new())
        .with_internal_api_base_url(Some(format!("http://{addr}")))
        .with_internal_capability_issuer(Arc::new(|_, _| {
            InternalHttpCapability::new("redirect-capability".to_string(), "tenant-a".to_string())
        }));
    let (status, _) =
        tokio_test::block_on(host.http_call("GET", &format!("http://{addr}/redirect"), &[], ""))
            .expect("redirect must be returned without following it");
    assert_eq!(status, 302);
    server.join().expect("server should finish");
}

#[test]
fn production_host_never_exposes_ambient_root_secret() {
    let mut secrets = BTreeMap::new();
    secrets.insert("temper_api_key".to_string(), "ambient-root".to_string());
    let host = ProductionWasmHost::new(secrets);

    let error = host
        .get_secret("temper_api_key")
        .expect_err("reserved root secret must not be available");
    assert!(error.contains("reserved"), "{error}");
}

#[test]
fn guest_log_span_attrs_include_message_and_invocation_context() {
    let context = WasmInvocationContext {
        tenant: "tenant-a".to_string(),
        entity_type: "Session".to_string(),
        entity_id: "ss-1".to_string(),
        trigger_action: "ContextReady".to_string(),
        wasm_module: Some("provider_caller".to_string()),
        trigger_params: serde_json::Value::Null,
        entity_state: serde_json::Value::Null,
        agent_id: Some("agent-1".to_string()),
        session_id: Some("ss-1".to_string()),
        integration_config: BTreeMap::new(),
        trace_id: "0123456789abcdef0123456789abcdef".to_string(),
        workflow_root_entity_type: None,
        workflow_root_entity_id: None,
        workflow_run_id: None,
        http_request: None,
    };

    let attrs = guest_log_span_attrs(
        "error",
        "provider failed",
        Some(&context),
        Some(r#"{"status":500}"#),
    );

    assert_eq!(attrs["log.severity"], "error");
    assert_eq!(attrs["log.message"], "provider failed");
    assert_eq!(attrs["tenant"], "tenant-a");
    assert_eq!(attrs["entity_type"], "Session");
    assert_eq!(attrs["entity_id"], "ss-1");
    assert_eq!(attrs["trigger_action"], "ContextReady");
    assert_eq!(attrs["agent_id"], "agent-1");
    assert_eq!(attrs["wasm_module"], "provider_caller");
    assert_eq!(attrs["gen_ai.conversation.id"], "ss-1");
    assert_eq!(attrs["trace_id"], "0123456789abcdef0123456789abcdef");
    assert_eq!(attrs["fields_json"], r#"{"status":500}"#);
}

#[test]
fn guest_log_span_event_is_named_for_trace_export() {
    #[derive(Clone)]
    struct EventCapture {
        names: Arc<Mutex<Vec<String>>>,
    }

    impl<S> tracing_subscriber::Layer<S> for EventCapture
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.names
                .lock()
                .expect("event capture lock poisoned")
                .push(event.metadata().name().to_string());
        }
    }

    let names = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(EventCapture {
        names: names.clone(),
    });
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    let context = WasmInvocationContext {
        tenant: "tenant-a".to_string(),
        entity_type: "Session".to_string(),
        entity_id: "ss-1".to_string(),
        trigger_action: "ContextReady".to_string(),
        wasm_module: Some("provider_caller".to_string()),
        trigger_params: serde_json::Value::Null,
        entity_state: serde_json::Value::Null,
        agent_id: Some("agent-1".to_string()),
        session_id: Some("ss-1".to_string()),
        integration_config: BTreeMap::new(),
        trace_id: "0123456789abcdef0123456789abcdef".to_string(),
        workflow_root_entity_type: None,
        workflow_root_entity_id: None,
        workflow_run_id: None,
        http_request: None,
    };

    {
        let span = tracing::info_span!("wasm.invoke");
        let _guard = span.enter();
        record_guest_log_span_event(
            "error",
            "provider failed",
            Some(&context),
            Some(r#"{"status":500}"#),
        );
    }

    let names = names.lock().expect("event capture lock poisoned");
    assert!(names.iter().any(|name| name == "wasm_guest.log"));
}

#[path = "tests/host_boundary_observability.rs"]
mod host_boundary_observability;
#[path = "tests/log_correlation.rs"]
mod log_correlation;
#[path = "tests/span_hint_tests.rs"]
mod span_hint_tests;

/// ARN-243 / ADR-0166. `host_emit_wide_event` is a second guest-authored
/// telemetry record with guest-chosen field names — the same untrusted channel as
/// the span APIs, and it reaches the backend directly.
#[test]
fn guest_wide_event_fields_drop_llm_content_for_non_opted_in_tenant() {
    use super::redact_guest_wide_event_fields;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    let mut tags = BTreeMap::new();
    tags.insert("gen_ai.prompt".to_string(), "SECRET PROMPT".to_string());
    tags.insert(
        "gen_ai.request.model".to_string(),
        "claude-opus-4-8".to_string(),
    );
    tags.insert("app.route".to_string(), "checkout".to_string());

    let mut attributes = BTreeMap::new();
    attributes.insert("gen_ai.completion".to_string(), json!("SECRET COMPLETION"));
    attributes.insert(
        "gen_ai.response.text".to_string(),
        json!("SECRET COMPLETION"),
    );
    attributes.insert("gen_ai.usage.input_tokens".to_string(), json!(42));
    attributes.insert("order.id".to_string(), json!("A-17"));

    redact_guest_wide_event_fields(&mut tags, &mut attributes, false);

    assert_eq!(
        tags.get("gen_ai.prompt"),
        None,
        "prompt tag must not export"
    );
    assert_eq!(
        tags.get("gen_ai.request.model"),
        Some(&"claude-opus-4-8".to_string())
    );
    assert_eq!(tags.get("app.route"), Some(&"checkout".to_string()));
    assert_eq!(attributes.get("gen_ai.completion"), None);
    assert_eq!(
        attributes.get("gen_ai.response.text"),
        None,
        "an unrecognized gen_ai.* key must not export just because it is unlisted"
    );
    assert_eq!(
        attributes.get("gen_ai.usage.input_tokens"),
        Some(&json!(42))
    );
    assert_eq!(attributes.get("order.id"), Some(&json!("A-17")));

    // Values inside recognized keys are bounded, so the prompt cannot ride along.
    let mut smuggle = BTreeMap::new();
    smuggle.insert("gen_ai.request.model".to_string(), json!("M".repeat(4096)));
    let mut no_tags = BTreeMap::new();
    redact_guest_wide_event_fields(&mut no_tags, &mut smuggle, false);
    let Some(Value::String(model)) = smuggle.get("gen_ai.request.model") else {
        panic!("metadata key should survive, bounded");
    };
    assert!(model.len() <= 256, "got {} bytes", model.len());
}

/// An opted-in tenant is unaffected by the wide-event filter.
#[test]
fn guest_wide_event_fields_are_untouched_when_opted_in() {
    use super::redact_guest_wide_event_fields;
    use serde_json::json;
    use std::collections::BTreeMap;

    let mut tags = BTreeMap::new();
    tags.insert("gen_ai.prompt".to_string(), "PROMPT".to_string());
    let mut attributes = BTreeMap::new();
    attributes.insert("gen_ai.completion".to_string(), json!("COMPLETION"));

    redact_guest_wide_event_fields(&mut tags, &mut attributes, true);

    assert_eq!(tags.get("gen_ai.prompt"), Some(&"PROMPT".to_string()));
    assert_eq!(
        attributes.get("gen_ai.completion"),
        Some(&json!("COMPLETION"))
    );
}
