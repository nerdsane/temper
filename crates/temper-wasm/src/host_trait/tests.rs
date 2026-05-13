use super::*;
use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::prelude::*;

#[test]
fn guest_metric_count_kind_is_counter() {
    assert!(guest_metric_is_counter_kind(Some("count")));
    assert!(guest_metric_is_counter_kind(Some("counter")));
    assert!(!guest_metric_is_counter_kind(Some("histogram")));
    assert!(!guest_metric_is_counter_kind(None));
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
fn internal_http_call_injects_bearer_even_with_explicit_principal_headers() {
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

    let mut secrets = BTreeMap::new();
    secrets.insert("temper_api_url".to_string(), format!("http://{addr}"));
    secrets.insert("temper_api_key".to_string(), "secret123".to_string());

    let host = ProductionWasmHost::new(secrets).with_invocation_context(WasmInvocationContext {
        tenant: "default".to_string(),
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
        ("X-Tenant-Id".to_string(), "default".to_string()),
        ("x-temper-principal-kind".to_string(), "agent".to_string()),
        ("x-temper-principal-id".to_string(), "system".to_string()),
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
        request.contains("authorization: bearer secret123"),
        "expected bearer token in request, got: {request}"
    );
    assert!(
        request.contains("x-temper-principal-kind: agent"),
        "expected explicit principal header to be preserved, got: {request}"
    );
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
fn internal_http_call_injects_bearer_from_internal_api_key_context() {
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

    let mut secrets = BTreeMap::new();
    secrets.insert("temper_api_url".to_string(), format!("http://{addr}"));

    let host = ProductionWasmHost::new(secrets)
        .with_internal_api_key(Some("ambient-secret".to_string()))
        .with_invocation_context(WasmInvocationContext {
            tenant: "default".to_string(),
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
            workflow_root_entity_type: None,
            workflow_root_entity_id: None,
            workflow_run_id: None,
            http_request: None,
        });

    let headers = vec![("X-Tenant-Id".to_string(), "default".to_string())];

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
        request.contains("authorization: bearer ambient-secret"),
        "expected ambient bearer token in request, got: {request}"
    );
}

#[test]
fn internal_http_call_uses_configured_internal_api_base_url_without_secret() {
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
        .with_internal_api_key(Some("ambient-secret".to_string()))
        .with_invocation_context(WasmInvocationContext {
            tenant: "default".to_string(),
            entity_type: "Session".to_string(),
            entity_id: "ss-1".to_string(),
            trigger_action: "WorkspaceReady".to_string(),
            wasm_module: Some("monty_repl".to_string()),
            trigger_params: Value::Null,
            entity_state: Value::Null,
            agent_id: Some("ss-1".to_string()),
            session_id: Some("ss-1".to_string()),
            integration_config: BTreeMap::new(),
            trace_id: String::new(),
            workflow_root_entity_type: None,
            workflow_root_entity_id: None,
            workflow_run_id: None,
            http_request: None,
        });

    let (status, _) = tokio_test::block_on(host.http_call(
        "GET",
        &format!("http://{addr}/tdata/Files('file-1')/$value"),
        &[("X-Tenant-Id".to_string(), "default".to_string())],
        "",
    ))
    .expect("internal call should succeed");

    assert_eq!(status, 200);
    server.join().expect("server thread");

    let request = captured.lock().expect("capture lock").to_lowercase();
    assert!(
        request.contains("authorization: bearer ambient-secret"),
        "expected ambient bearer token in request, got: {request}"
    );
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

mod host_boundary_observability;
mod log_correlation;
mod span_hint_tests;
